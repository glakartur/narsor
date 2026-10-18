//! Compilation of a YAML workflow file into the domain model.
//!
//! The loader reads the file, deserializes it into [`crate::workflow::schema`]
//! structures and then compiles those into a [`WorkflowDefinition`]:
//!
//! * `globals.timeout` and `globals.retry_policy` are propagated as per-state
//!   defaults; a value declared on the state itself always wins,
//! * `terminal_states` is turned into the `terminal` flag on each named state,
//!   together with the `terminal_status` the entry declares (§3.5),
//! * `initial_state` is the state flagged `initial: true`, or — with no such
//!   flag — the first state in file order,
//! * `globals.workspace` becomes a single `WorkspaceSpec` on the definition,
//!   because a workspace belongs to an execution, not to a state.
//!
//! No graph rules are checked here: an entry of `terminal_states` that names a
//! state which does not exist is carried through without complaint, and
//! reporting it is the validator's job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::domain::{
    ActionConfig, AgentActionConfig, AgentDecisionConfig, DecisionConfig, DisplayLinkSpec,
    DisplaySpec, GlobalsConfig, HumanActionConfig, HumanDecisionConfig, HumanInputField,
    HumanInputType, InputParameter, OutputDefinition, ParamType, RetryPolicy, ScriptActionConfig,
    State, TerminalStatus, WaitActionConfig, WorkflowDefinition, WorkspaceSpec,
};

use super::schema::{
    ActionSchema, DecisionSchema, DisplaySchema, GlobalsSchema, HumanInputTypeSchema, InputSchema,
    ParamTypeSchema, RetryPolicySchema, StateSchema, TerminalStatusSchema, WorkflowFile,
    WorkspaceSchema,
};

/// Errors that can occur while loading a workflow definition from YAML.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read workflow file '{path}'")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse workflow file '{path}': {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("failed to parse workflow definition: {0}")]
    ParseInline(#[source] serde_yaml::Error),

    #[error("workflow '{0}' declares no states")]
    NoStates(String),
}

/// Loads and compiles a workflow definition from a YAML file.
pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<WorkflowDefinition, LoadError> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let file: WorkflowFile =
        serde_yaml::from_str(&contents).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    compile(file)
}

/// Loads and compiles a workflow definition from a YAML string.
pub fn load_from_str(yaml: &str) -> Result<WorkflowDefinition, LoadError> {
    let file: WorkflowFile = serde_yaml::from_str(yaml).map_err(LoadError::ParseInline)?;
    compile(file)
}

/// Compiles an already deserialized workflow file into the domain model.
pub fn compile(file: WorkflowFile) -> Result<WorkflowDefinition, LoadError> {
    if file.states.is_empty() {
        return Err(LoadError::NoStates(file.name));
    }

    let globals = file.globals.unwrap_or_default();
    let default_timeout_secs = globals.timeout.map(|d| d.as_secs());
    let default_retry_policy = globals.retry_policy.as_ref().map(compile_retry_policy);
    let workspace = globals.workspace.as_ref().map(compile_workspace);

    let initial_state = file
        .states
        .iter()
        .find(|(_, state)| state.initial)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| file.states[0].0.clone());

    let mut states = HashMap::with_capacity(file.states.len());
    for (id, state) in file.states {
        let terminal = file.terminal_states.contains(&id);
        let terminal_status = file
            .terminal_states
            .status_of(&id)
            .map(compile_terminal_status);
        let compiled = compile_state(
            id.clone(),
            state,
            terminal,
            terminal_status,
            default_timeout_secs,
            default_retry_policy.as_ref(),
        );
        states.insert(id, compiled);
    }

    Ok(WorkflowDefinition {
        // The YAML contract has no id; the name is declared unique, so it is
        // the only stable identifier available at load time.
        id: file.name.clone(),
        name: file.name,
        version: file.version,
        description: file.description,
        inputs: file.inputs.into_iter().map(compile_input).collect(),
        globals: compile_globals(&globals),
        workspace,
        display: file.display.map(compile_display),
        states,
        initial_state,
    })
}

fn compile_globals(globals: &GlobalsSchema) -> GlobalsConfig {
    GlobalsConfig {
        timeout_secs: globals.timeout.map(|d| d.as_secs()),
        retry_policy: globals.retry_policy.as_ref().map(compile_retry_policy),
        image: globals.image.clone(),
    }
}

fn compile_display(display: DisplaySchema) -> DisplaySpec {
    DisplaySpec {
        title: display.title,
        links: display
            .links
            .into_iter()
            .map(|link| DisplayLinkSpec {
                label: link.label,
                text: link.text,
                url: link.url,
            })
            .collect(),
    }
}

fn compile_workspace(workspace: &WorkspaceSchema) -> WorkspaceSpec {
    match workspace {
        WorkspaceSchema::Git(git) => WorkspaceSpec::Git {
            repository: git.repository.clone(),
            git_ref: git.git_ref.clone(),
            preserve_paths: git.preserve_paths.clone(),
        },
        WorkspaceSchema::Local(local) => WorkspaceSpec::Local {
            path: local.path.clone(),
            preserve_paths: local.preserve_paths.clone(),
        },
    }
}

fn compile_retry_policy(policy: &RetryPolicySchema) -> RetryPolicy {
    RetryPolicy {
        max_attempts: policy.max_attempts,
        backoff_secs: policy.backoff.map(|d| d.as_secs()).unwrap_or(0),
        ..RetryPolicy::default()
    }
}

fn compile_input(input: InputSchema) -> InputParameter {
    InputParameter {
        name: input.name,
        param_type: match input.param_type {
            ParamTypeSchema::String => ParamType::String,
            ParamTypeSchema::Text => ParamType::Text,
            ParamTypeSchema::Integer => ParamType::Integer,
            ParamTypeSchema::Boolean => ParamType::Boolean,
            ParamTypeSchema::Repository => ParamType::Repository,
        },
        required: input.required,
        default: input.default,
        description: input.description,
    }
}

/// Maps a declared status from the YAML layer onto the domain enum.
fn compile_terminal_status(status: TerminalStatusSchema) -> TerminalStatus {
    match status {
        TerminalStatusSchema::Completed => TerminalStatus::Completed,
        TerminalStatusSchema::Failed => TerminalStatus::Failed,
        TerminalStatusSchema::Cancelled => TerminalStatus::Cancelled,
    }
}

fn compile_state(
    id: String,
    state: StateSchema,
    terminal: bool,
    terminal_status: Option<TerminalStatus>,
    default_timeout_secs: Option<u64>,
    default_retry_policy: Option<&RetryPolicy>,
) -> State {
    let timeout_secs = state.timeout.map(|d| d.as_secs()).or(default_timeout_secs);

    let retry_policy = state
        .retry_policy
        .as_ref()
        .map(compile_retry_policy)
        .or_else(|| default_retry_policy.cloned());

    State {
        id,
        max_entries: state.max_entries,
        description: state.description,
        action: state.action.map(compile_action),
        outputs: state
            .outputs
            .into_iter()
            .map(|output| OutputDefinition {
                label: output.label,
                requires_variables: output
                    .requires_variables
                    .into_iter()
                    .map(|variable| match variable {
                        crate::workflow::schema::RequiredVariableSchema::Name(name) => {
                            crate::domain::RequiredVariable::Name(name)
                        }
                        crate::workflow::schema::RequiredVariableSchema::Described {
                            name,
                            description,
                        } => crate::domain::RequiredVariable::Described { name, description },
                    })
                    .collect(),
                // A blank description says nothing; treating it as absent
                // keeps an empty dash out of every place labels are listed.
                description: output
                    .description
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty()),
                requires_human_approval: output.requires_human_approval,
                resume_comment_field: output
                    .resume_comment_field
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty()),
            })
            .collect(),
        transitions: state.transitions,
        decision: state.decision.map(compile_decision),
        timeout_secs,
        retry_policy,
        terminal,
        terminal_status,
        allow_manual_override: state.allow_manual_override,
        image: state.image,
    }
}

fn compile_action(action: ActionSchema) -> ActionConfig {
    match action {
        ActionSchema::Agent(agent) => ActionConfig::Agent(AgentActionConfig {
            harness: agent.harness,
            permission_grants: Vec::new(),
            model: agent.model,
            workspace_access: agent.workspace_access,
            prompt: agent.prompt,
            // The YAML contract has no per-action workspace (§6 lists it as a
            // future extension), so this stays unset.
            workspace: None,
            parameters: agent.parameters,
            read_only_paths: agent.read_only_paths,
        }),
        ActionSchema::Script(script) => ActionConfig::Script(ScriptActionConfig {
            command: script.command,
            args: script.args,
            working_dir: script.working_dir,
            env: script.env,
            exit_codes: script.exit_codes,
            timeout_secs: script.timeout.map(|d| d.as_secs()),
        }),
        ActionSchema::Human(human) => ActionConfig::Human(HumanActionConfig {
            // Name drift: YAML says `message`, the domain says `instruction`.
            instruction: human.message,
            inputs: human
                .inputs
                .into_iter()
                .map(|field| HumanInputField {
                    name: field.name,
                    field_type: match field.field_type {
                        HumanInputTypeSchema::Text => HumanInputType::Text,
                        HumanInputTypeSchema::Select => HumanInputType::Select,
                    },
                    options: field.options,
                    required: field.required,
                    description: field.description,
                })
                .collect(),
            input_schema: None,
        }),
        ActionSchema::Wait(wait) => ActionConfig::Wait(WaitActionConfig {
            // Name drift: YAML says `event_type`, the domain says `event`.
            event: wait.event_type,
            filter: wait.filter,
            timeout_secs: wait.timeout.map(|d| d.as_secs()),
        }),
    }
}

fn compile_decision(decision: DecisionSchema) -> DecisionConfig {
    match decision {
        DecisionSchema::Rules => DecisionConfig::Rules,
        DecisionSchema::Agent(agent) => DecisionConfig::Agent(AgentDecisionConfig {
            harness: agent.harness,
            prompt: agent.prompt,
        }),
        DecisionSchema::Human(human) => DecisionConfig::Human(HumanDecisionConfig {
            message: human.message,
        }),
    }
}
