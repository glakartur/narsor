use super::*;
use crate::persistence::DefaultHarnessSelector;

/// Where the harness of an agent action that names none comes from.
///
/// Shared by every engine: the daemon's reads its live
/// [`DefaultHarnessSelector`], the runner's a name of its own. Read once per
/// action so the selected harness stays pinned across every attempt.
pub trait DefaultHarnessSource: Send + Sync + std::fmt::Debug {
    /// The default harness at this moment.
    fn current_harness(&self) -> String;
}

impl DefaultHarnessSource for String {
    fn current_harness(&self) -> String {
        self.clone()
    }
}

impl DefaultHarnessSource for DefaultHarnessSelector {
    fn current_harness(&self) -> String {
        self.current().effective
    }
}

pub(super) fn new_session(
    config: &AgentActionConfig,
    harness: &str,
    execution_id: Option<&str>,
) -> AgentSession {
    AgentSession {
        id: Uuid::new_v4().to_string(),
        workflow_execution_id: execution_id.unwrap_or_default().to_string(),
        harness: harness.to_string(),
        started_at: Utc::now(),
        ended_at: None,
        status: AgentSessionStatus::Starting,
        model: config.model.clone(),
        config: serde_json::json!({
            "harness": harness,
            "read_only_paths": config.read_only_paths,
            "parameters": config.parameters,
        }),
    }
}

pub(super) fn resume_session(config: &AgentActionConfig) -> Result<Option<&str>, ActionError> {
    match config.parameters.get(RESUME_SESSION_PARAMETER) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(session)) if session.is_empty() => Ok(None),
        Some(Value::String(session)) => Ok(Some(session.as_str())),
        Some(other) => Err(ActionError {
            message: format!(
                "parameter '{RESUME_SESSION_PARAMETER}' must be a session id string, got {other}"
            ),
            error_type: Some("invalid_parameter".to_string()),
            details: None,
        }),
    }
}

/// Builds the output contract appended to every prompt, so the agent is told
/// the contract it has to meet.
///
/// Kept deliberately short and imperative: it competes for attention with the
/// state's own prompt, and a long preamble is the first thing a model drops.
///
/// Everything listed comes from the state itself (KTD1): its variables and
/// artifacts, each described once, then its choices in declaration order,
/// each with its description and with the variables and the artifacts it
/// requires (R10). The engine is therefore the single source of the list: a
/// prompt no longer has to repeat the labels for the agent to know them.
/// With no choices to list (a direct call with nothing wired up) the agent
/// is pointed at the label the task asks for instead.
///
/// `harness_note` is how the harness wants the answer delivered
/// ([`HarnessAdapter::contract_note`]); it closes the contract.
pub fn output_contract(contract: &StepContract<'_>, harness_note: Option<&str>) -> String {
    format!(
        "\n\n\
---\n\
When you are done, end your reply with a single JSON object on its own, and \
nothing after it:\n\
{{\"choice\": \"<label>\", \"reason\": \"<one line>\", \"variables\": {{}}, \
\"artifacts\": {{}}, \"additional_data\": {{}}, \"additional_artifacts\": []}}\n\
{definitions}\
{choices}\
`reason` is required: one line saying why you made this choice.\n\
`additional_data` (any object) and `additional_artifacts` (a list of paths) hold \
anything else worth keeping; the workflow does not interpret them. The other \
fields are optional unless your choice requires them. If, and only if, you need a \
person to answer a question before continuing this same task, return instead:\n\
{{\"status\":\"awaiting_input\",\"question\":\"<question>\",\"options\":[\"<optional choice>\"]}}\n\
Do not use `awaiting_input` for a harness, sandbox, permission, or tool error; \
those are action failures. Send the answer with `choice` exactly once, when all \
your subagents have returned their results. A decision you cannot make yourself (a trade-off, a \
change of behaviour, or a question a skill would put to the user) is a question \
for the person: ask it with `awaiting_input`, and do not use a question tool. Do \
not omit the JSON object: without it the workflow cannot continue.\
{note}",
        definitions = step_definitions(contract),
        choices = choice_list(contract.choices),
        note = harness_note
            .map(|note| format!("\n{note}"))
            .unwrap_or_default(),
    )
}

/// The step's variables and artifacts, each with what it is.
fn step_definitions(contract: &StepContract<'_>) -> String {
    let mut text = String::new();
    if !contract.variables.is_empty() {
        text.push_str("The step's variables (in `variables`, name → value):\n");
        for variable in contract.variables {
            text.push_str(&described_item(&variable.name, Some(&variable.description)));
        }
    }
    if !contract.artifacts.is_empty() {
        text.push_str("The step's artifacts (in `artifacts`, name → path):\n");
        for artifact in contract.artifacts {
            text.push_str(&described_item(&artifact.name, Some(&artifact.description)));
        }
    }
    text
}

/// The labels `choice` may take, each with what it requires.
fn choice_list(choices: &[ChoiceDefinition]) -> String {
    if choices.is_empty() {
        return "`choice` is required and must be the label the task asks for.\n".to_string();
    }
    let mut text = "`choice` is required and must be exactly one of these labels:\n".to_string();
    for choice in choices {
        text.push_str(&described_item(
            &choice.label,
            choice.description.as_deref(),
        ));
        text.push_str(&required_names("variables", &choice.requires_variables));
        text.push_str(&required_names("artifacts", &choice.requires_artifacts));
    }
    text
}

/// `- `name` — description`, or the bare name when the description is blank.
fn described_item(name: &str, description: Option<&str>) -> String {
    match description.map(str::trim) {
        Some(description) if !description.is_empty() => {
            format!("- `{name}` — {description}\n")
        }
        _ => format!("- `{name}`\n"),
    }
}

/// The line naming what a choice requires of one kind, empty for none.
fn required_names(kind: &str, names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let names: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    format!("  - Required {kind}: {}\n", names.join(", "))
}

/// The `status` of an answer that asks a person before the task continues.
pub(super) const AWAITING_INPUT: &str = "awaiting_input";

/// The `error_type` of an answer no output label could be extracted from — a
/// format error the engine answers with a reprompt, not with `retry_policy`
/// (KTD7).
pub const MISSING_OUTPUT_LABEL: &str = "missing_output_label";

/// What the engine asks an agent whose answer carried no usable output label
/// (KTD7), in the same harness session, before the output contract.
///
/// Generic on purpose: the reprompt only repairs the format of work already
/// done, so it tells the agent not to carry on working. The reason the
/// previous answer was rejected is quoted after it.
pub const OUTPUT_REPROMPT: &str = "Your previous reply could not be used by the workflow, \
because it did not end with a valid output object. Do not continue the task and do not \
change any files: the work is already done. Reply only with the JSON object described \
below, choosing the label that describes the work you already did.";

/// The configuration of the engine's reprompt of an agent (KTD7).
///
/// Resumes `harness_session` on `harness` — the harness the rejected attempt
/// ran on, never the current default, which may have changed since — with
/// read-only workspace access, whatever the workflow declares: fixing the
/// format of an answer is not further work. Any `resume_session` of the
/// workflow's own is replaced by the session being repaired. `reason` says
/// why the previous answer was rejected.
pub fn reprompt_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    reason: &str,
) -> AgentActionConfig {
    let mut config = original.clone();
    config.harness = Some(harness.to_string());
    config.workspace_access = Some(WorkspaceAccess::ReadOnly);
    config.prompt = format!("{OUTPUT_REPROMPT}\n\nWhy it was rejected: {reason}");
    config.parameters.insert(
        RESUME_SESSION_PARAMETER.to_string(),
        Value::String(harness_session.to_string()),
    );
    config
}

/// What follows the conductor's own words when it asks the agent again
/// (F1): the same boundaries as the engine's reprompt.
pub const CONDUCTOR_REPROMPT_GUARD: &str = "Do not continue the task and do not change any \
files. Reply only with the JSON object described below.";

/// The configuration of the conductor's reprompt of an agent (F1, R17):
/// [`reprompt_config`] with the conductor's `prompt` in place of the
/// engine's generic one.
pub fn conductor_reprompt_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    prompt: &str,
) -> AgentActionConfig {
    let mut config = reprompt_config(original, harness, harness_session, "");
    config.prompt = format!("{}\n\n{CONDUCTOR_REPROMPT_GUARD}", prompt.trim());
    config
}

/// The harness used when neither the action nor the configuration names one.
///
/// The name the adapter itself registers under, not a second spelling of it:
/// a default that `HarnessRegistry::get` does not know would fail at run time
/// rather than at compile time.
pub const BUILT_IN_HARNESS: &str = super::super::harness::claude_code::CLAUDE_CODE;

/// The `parameters` key that continues an existing harness session (D18).
pub const RESUME_SESSION_PARAMETER: &str = "resume_session";

/// The harness refused a saved session id. The runner turns this into a new
/// operator choice instead of retrying it or discarding the drafted message.
pub const RESUME_SESSION_REJECTED: &str = "resume_session_rejected";

/// Configures the next normal attempt as a continuation of an operator-facing
/// conversation. Unlike [`reprompt_config`], this preserves the action's
/// workspace permission: the operator is explicitly asking the agent to keep
/// working, not merely to repair an output envelope.
pub fn continuation_config(
    original: &AgentActionConfig,
    harness: &str,
    harness_session: &str,
    message: &str,
) -> AgentActionConfig {
    let mut config = original.clone();
    config.harness = Some(harness.to_string());
    config.parameters.insert(
        RESUME_SESSION_PARAMETER.to_string(),
        Value::String(harness_session.to_string()),
    );
    config.prompt = message.to_string();
    config
}

/// The variable the harness session id is published as, when there is one.
pub const SESSION_ID_VARIABLE: &str = "agent_session_id";

/// The `data` key carrying the session record of the run.
pub const SESSION_DATA_KEY: &str = "agent_session";

/// The most a single stdout line is handed to the harness's stream parser.
///
/// Every line reaches the conversation file in full regardless (R14) — this
/// only bounds what is fed to [`HarnessStreamParser::on_stdout_line`]. Tens of
/// megabytes is well past anything a harness's own JSON envelope needs (a
/// multi-megabyte `tool_result` line is ordinary; the final `result`/`text`
/// line is not), so a line past this limit is bulky tool output, not the
/// answer, and is safely skipped for parsing.
pub const PARSE_LINE_LIMIT: usize = 32 * 1024 * 1024;
