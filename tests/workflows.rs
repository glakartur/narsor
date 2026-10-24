//! Tests for the shipped workflow definitions in `workflows/`.
//!
//! These are the two pipelines narsor ships with — the compound
//! engineering chain and the Open Mercato SDLC — and they are user-facing
//! artifacts, so they are held to the same bar as code: they validate with no
//! violations, their human gates really can hand a label back to the engine,
//! their agent prompts carry no hand-written label list while the engine's
//! contract names every declared label with its description, and each one runs
//! end to end.
//!
//! **No real harness and no real `git`, `gh` or `glab` is ever run here.** The
//! agent states, `finalize` included, are driven by
//! `tests/fixtures/harness/fake_harness.sh` through
//! [`ClaudeCodeAdapter::with_program`], the way `tests/actions_agent.rs` does
//! it. A test cannot check what a real agent does on a repository host, so the
//! `finalize` tests check the step contract and the transitions that follow
//! each answer. The script states run in a temporary workspace: `verify_work`
//! finds no `scripts/verify.sh` there. Nothing is ever written into the
//! repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use narsor_engine::domain::{
    ActionConfig, EventData, ExecutionStatus, HumanInputType, State, TerminalStatus,
    WorkflowDefinition, Workspace,
};
use narsor_engine::engine::actions::agent::output_contract;
use narsor_engine::engine::StepContract;
use narsor_engine::engine::{
    ActionKind, AgentExecutor, ClaudeCodeAdapter, Engine, HarnessRegistry, HumanInputDelivery,
};
use narsor_engine::workflow::{load_from_file, validate_path};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The names of the workflows this crate ships, and their files.
const SHIPPED: [(&str, &str); 4] = [
    ("compound-engineering", "compound-engineering.yaml"),
    ("compound-engineering-chat", "compound-engineering-chat.yaml"),
    ("compound-engineering-debug", "compound-engineering-debug.yaml"),
    ("open-mercato-sdlc", "open-mercato-sdlc.yaml"),
];

/// The `workflows/` directory of the repository.
fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows")
}

/// A shipped workflow, compiled.
fn shipped(file: &str) -> WorkflowDefinition {
    load_from_file(workflows_dir().join(file))
        .unwrap_or_else(|error| panic!("load shipped workflow '{file}': {error}"))
}

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-workflows-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create directory");
        path
    }

    /// The workspace the actions run in.
    fn workspace(&self) -> Workspace {
        Workspace::local_folder(self.dir("workspace").to_string_lossy().into_owned())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The fake harness CLI, installed in a directory of its own and told what to
/// answer on each call.
struct FakeHarness {
    program: PathBuf,
}

impl FakeHarness {
    /// Installs the fixture script and scripts its answers, one output label
    /// per call, in order.
    ///
    /// Copies to a temporary name and renames it into place — see the
    /// comment on `FakeHarness::install` in `tests/actions_agent.rs` for why
    /// a straight `fs::copy` onto the final name is an intermittent ETXTBSY
    /// flake under parallel test threads.
    fn installed_in(dir: PathBuf, labels: &[&str]) -> Self {
        let answers: Vec<Value> = labels
            .iter()
            .map(|label| json!({ "choice": label }))
            .collect();
        Self::answering_in(dir, &answers)
    }

    /// Installs the fixture script and scripts one whole answer object (choice,
    /// `variables`, ...) per call, in order.
    fn answering_in(dir: PathBuf, answers: &[Value]) -> Self {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        let temp = dir.join(format!(".fake_harness.sh.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");

        for (index, result) in answers.iter().enumerate() {
            let answer = json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "session_id": "fake-session-1",
                "result": result.to_string(),
            })
            .to_string();
            fs::write(dir.join(format!("stdout.{}", index + 1)), answer)
                .expect("write the harness answer");
        }

        Self { program }
    }

    /// An engine whose `agent` executor runs the fake harness.
    fn engine(&self, store_root: PathBuf) -> Engine {
        let executor =
            AgentExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(Arc::new(
                ClaudeCodeAdapter::with_program(self.program.to_string_lossy().into_owned()),
            )));
        Engine::new(narsor_engine::persistence::ExecutionStore::new(store_root))
            .with_executor(ActionKind::Agent, Arc::new(executor))
    }
}

/// An answer to one human gate: the state it belongs to and the field values.
#[derive(Clone)]
struct Answer {
    state: &'static str,
    values: Vec<(&'static str, &'static str)>,
    approve: bool,
}

impl Answer {
    fn new(state: &'static str, values: &[(&'static str, &'static str)]) -> Self {
        Self {
            state,
            values: values.to_vec(),
            approve: false,
        }
    }

    fn delivery(&self) -> HumanInputDelivery {
        let mut values = Map::new();
        for (name, value) in &self.values {
            values.insert((*name).to_string(), Value::String((*value).to_string()));
        }
        let delivery = HumanInputDelivery::with_values(values);
        if self.approve {
            delivery.with_action(narsor_engine::engine::actions::human::HumanInputAction::Approve)
        } else {
            delivery
        }
    }
}

/// Every `agent` state of a definition, by id.
fn agent_states(definition: &WorkflowDefinition) -> Vec<(&String, &State)> {
    definition
        .states
        .iter()
        .filter(|(_, state)| matches!(state.action, Some(ActionConfig::Agent(_))))
        .collect()
}

/// Every `human` state of a definition, by id.
fn human_states(definition: &WorkflowDefinition) -> Vec<(&String, &State)> {
    definition
        .states
        .iter()
        .filter(|(_, state)| matches!(state.action, Some(ActionConfig::Human(_))))
        .collect()
}

/// The prompt an agent state actually sends: its own prompt with the output
/// contract the engine appends.
fn built_prompt(state: &State) -> String {
    let Some(ActionConfig::Agent(config)) = &state.action else {
        panic!("built_prompt is only defined for agent states");
    };
    format!(
        "{}{}",
        config.prompt,
        output_contract(&StepContract::of(state))
    )
}

/// The declared output labels of a state.
fn labels(state: &State) -> Vec<String> {
    state
        .choices
        .iter()
        .map(|output| output.label.clone())
        .collect()
}

/// Every `(state, label, target)` edge of a definition, sorted.
fn edges(definition: &WorkflowDefinition) -> Vec<(&str, &str, &str)> {
    let mut edges: Vec<(&str, &str, &str)> = definition
        .states
        .iter()
        .flat_map(|(id, state)| {
            state
                .transitions
                .iter()
                .map(move |(label, target)| (id.as_str(), label.as_str(), target.as_str()))
        })
        .collect();
    edges.sort_unstable();
    edges
}

fn sorted<'a>(edges: &[(&'a str, &'a str, &'a str)]) -> Vec<(&'a str, &'a str, &'a str)> {
    let mut edges = edges.to_vec();
    edges.sort_unstable();
    edges
}

// ---------------------------------------------------------------------------
// The contract every shipped workflow keeps
// ---------------------------------------------------------------------------

#[test]
fn every_shipped_workflow_validates_without_a_single_violation() {
    for (name, file) in SHIPPED {
        let violations = validate_path(workflows_dir().join(file))
            .unwrap_or_else(|error| panic!("'{file}' does not even load: {error}"));
        assert!(
            violations.is_empty(),
            "'{name}' has validation violations: {violations:?}"
        );
    }
}

#[test]
fn shipped_workflows_use_finished_not_failed_as_a_workflow_terminal() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);

        assert!(
            definition.states.contains_key("finished"),
            "'{name}' has no finished state"
        );
        assert!(
            !definition.states.contains_key("failed"),
            "'{name}' has a failed pseudo-state"
        );
        assert_eq!(
            definition
                .states
                .get("finished")
                .and_then(|state| state.terminal_status),
            Some(TerminalStatus::Completed),
            "'{name}' does not complete through finished"
        );
        assert!(
            definition
                .states
                .values()
                .flat_map(|state| state.transitions.values())
                .all(|target| target != "failed"),
            "'{name}' transitions into a pseudo-state named 'failed'"
        );
    }
}

/// The trap of `WORKFLOW_SCHEMA.md` §3.3.C: a `text` field carries no output
/// label, so a gate that only asks for prose stalls the workflow forever. A
/// gate is only a gate if it hands a label back, and the engine derives one
/// from a single `select` field whose value is the label.
#[test]
fn every_human_gate_offers_a_select_over_exactly_its_outputs() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        let gates = human_states(&definition);
        assert!(
            !gates.is_empty(),
            "'{name}' has no human gate at all — an autonomous pipeline with no \
             human gate drives straight to a merge"
        );

        for (id, state) in gates {
            let Some(ActionConfig::Human(config)) = &state.action else {
                unreachable!("filtered to human states");
            };

            let selects: Vec<_> = config
                .inputs
                .iter()
                .filter(|field| field.field_type == HumanInputType::Select)
                .collect();
            assert_eq!(
                selects.len(),
                1,
                "'{name}' gate '{id}' must carry exactly one select field; \
                 none means the gate stalls, two means no label can be derived"
            );

            let select = selects[0];
            assert!(
                select.required,
                "'{name}' gate '{id}': the deciding select field must be required"
            );
            assert_eq!(
                select.options,
                labels(state),
                "'{name}' gate '{id}': the select options and the state's outputs \
                 must be the same list, in the same order"
            );
        }
    }
}

/// The executor refuses to guess a label, so the agent has to be told every
/// label its state declares. The engine does that: it appends the output
/// contract built from `outputs`. What the agent actually receives — the
/// state's prompt plus that contract — must therefore name every declared
/// label together with its description.
#[test]
fn every_built_agent_prompt_names_every_label_with_its_description() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        for (id, state) in agent_states(&definition) {
            let prompt = built_prompt(state);
            for output in &state.choices {
                let description = output.description.as_deref().unwrap_or_else(|| {
                    panic!(
                        "'{name}' state '{id}': label '{}' has no description",
                        output.label
                    )
                });
                let line = format!("- `{}` — {}", output.label, description.trim());
                assert!(
                    prompt.contains(&line),
                    "'{name}' state '{id}': the built prompt never lists '{line}'"
                );
            }
        }
    }
}

/// The label list lives in `outputs` and nowhere else. A prompt that repeats
/// it by hand drifts from `outputs` in the first edit that touches only one of
/// the two, so no built prompt may carry a second list next to the engine's.
#[test]
fn no_agent_prompt_carries_a_manual_label_list() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        for (id, state) in agent_states(&definition) {
            let prompt = built_prompt(state);
            assert_eq!(
                prompt
                    .to_lowercase()
                    .matches("exactly one of these labels")
                    .count(),
                1,
                "'{name}' state '{id}': only the engine's contract may introduce \
                 the label list"
            );
            for label in labels(state) {
                let quoted = format!("- \"{label}\"");
                assert!(
                    !prompt.contains(&quoted),
                    "'{name}' state '{id}': label '{label}' is listed by hand as '{quoted}'"
                );
                let contract_bullet = format!("- `{label}`");
                assert_eq!(
                    prompt.matches(&contract_bullet).count(),
                    1,
                    "'{name}' state '{id}': label '{label}' must be listed once, by the \
                     engine's contract"
                );
            }
        }
    }
}

/// An agent that stalls waiting for an approval it will never get would
/// otherwise hang the execution for good.
#[test]
fn every_agent_state_has_a_time_limit() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        let global = definition.globals.timeout_secs;
        for (id, state) in agent_states(&definition) {
            assert!(
                state.timeout_secs.or(global).is_some(),
                "'{name}' state '{id}' has no timeout of its own and no global one"
            );
        }
    }
}

/// §3.5: the list form of `terminal_states` means `completed` for every entry,
/// so shipped workflows must use the mapping form.  A workflow can intentionally
/// have no failure terminal: executor failures retain the active state and set
/// the execution status to `failed` directly.
#[test]
fn terminals_are_declared_in_the_mapping_form() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        let terminals: Vec<(&String, &State)> = definition
            .states
            .iter()
            .filter(|(_, state)| state.terminal)
            .collect();
        assert!(!terminals.is_empty(), "'{name}' declares no terminal state");

        for (id, state) in &terminals {
            assert!(
                state.terminal_status.is_some(),
                "'{name}' terminal '{id}' carries no declared status — the list \
                 form of terminal_states would report it as completed"
            );
            assert!(
                state.action.is_none(),
                "'{name}' terminal '{id}' must carry no action"
            );
        }
    }
}

/// §5.1: no deadlocks — a terminal is reachable from every state.
#[test]
fn a_terminal_is_reachable_from_every_state() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);

        for (id, state) in &definition.states {
            if state.terminal {
                continue;
            }

            let mut seen = vec![id.clone()];
            let mut queue = vec![id.clone()];
            let mut reached_terminal = false;

            while let Some(current) = queue.pop() {
                let current = definition
                    .states
                    .get(&current)
                    .unwrap_or_else(|| panic!("'{name}': state '{current}' is missing"));
                if current.terminal {
                    reached_terminal = true;
                    break;
                }
                for target in current.transitions.values() {
                    if !seen.contains(target) {
                        seen.push(target.clone());
                        queue.push(target.clone());
                    }
                }
            }

            assert!(
                reached_terminal,
                "'{name}': no terminal state is reachable from '{id}'"
            );
        }
    }
}

/// Every state is reachable from the initial one — a state nobody can enter is
/// a state nobody maintains.
#[test]
fn every_state_is_reachable_from_the_initial_state() {
    for (name, file) in SHIPPED {
        let definition = shipped(file);
        let mut seen = vec![definition.initial_state.clone()];
        let mut queue = vec![definition.initial_state.clone()];

        while let Some(current) = queue.pop() {
            let state = definition
                .states
                .get(&current)
                .unwrap_or_else(|| panic!("'{name}': state '{current}' is missing"));
            for target in state.transitions.values() {
                if !seen.contains(target) {
                    seen.push(target.clone());
                    queue.push(target.clone());
                }
            }
        }

        for id in definition.states.keys() {
            assert!(
                seen.contains(id),
                "'{name}': state '{id}' cannot be reached from '{}'",
                definition.initial_state
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Full runs
// ---------------------------------------------------------------------------

/// Drives a definition from its initial state to a terminal, answering each
/// human gate in turn, and asserts the gates came up in the expected order.
async fn run_to_terminal(
    engine: &Engine,
    definition: &WorkflowDefinition,
    inputs: Value,
    workspace: Workspace,
    answers: &[Answer],
) -> narsor_engine::domain::WorkflowExecution {
    let mut execution = engine
        .start_and_run(definition, inputs, workspace)
        .await
        .expect("start the workflow");

    for answer in answers {
        assert_eq!(
            execution.status,
            ExecutionStatus::InputNeeded,
            "expected the run to pause at '{}', it is {:?} in {:?}",
            answer.state,
            execution.status,
            execution.current_state
        );
        let pending = engine
            .pending_input(&execution.id)
            .expect("read the pending request")
            .expect("a request is pending");
        assert_eq!(
            pending.state_id, answer.state,
            "the pipeline paused at an unexpected gate; pending request: {pending:#?}"
        );

        execution = engine
            .deliver_human_input(definition, &execution.id, &answer.delivery())
            .await
            .expect("deliver the answer to the gate");
    }

    execution
}

// ---------------------------------------------------------------------------
// The compound engineering pipeline
// ---------------------------------------------------------------------------

/// The pull request every publish step reports. `finalize` reads its number
/// and project from this URL, never from a branch name.
const PR_URL: &str = "https://gitlab.example.test/acme/web/-/merge_requests/7";
const REQUIREMENTS_DOC: &str = "docs/brainstorms/2026-10-21-subscribe-requirements.md";
const PLAN_DOC: &str = "docs/plans/2026-10-21-subscribe-plan.md";
const LEARNING_DOC: &str = "docs/solutions/2026-10-21-subscribe-learning.md";

/// One agent answer in the reply contract: a choice with its reason.
fn reply(choice: &str) -> Value {
    json!({ "choice": choice, "reason": format!("{choice}: the step's work is done") })
}

/// One agent answer that also sets `variables` or names `artifacts`.
fn reply_with(choice: &str, fields: Value) -> Value {
    let mut answer = reply(choice);
    for (key, value) in fields.as_object().expect("the fields are an object") {
        answer[key] = value.clone();
    }
    answer
}

fn requirements_drafted() -> Value {
    reply_with(
        "requirements_drafted",
        json!({
            "artifacts": { "requirements_doc": REQUIREMENTS_DOC },
            "variables": { "requirements_blocker": "" },
        }),
    )
}

fn reviewed() -> Value {
    reply_with(
        "reviewed",
        json!({ "variables": { "review_summary": "2 minor findings, both fixed, none left" } }),
    )
}

fn published() -> Value {
    reply_with("published", json!({ "variables": { "pr_url": PR_URL } }))
}

fn plan_ready() -> Value {
    reply_with(
        "plan_ready",
        json!({
            "artifacts": { "plan_doc": PLAN_DOC },
            "variables": { "plan_blocker": "" },
        }),
    )
}

fn implemented() -> Value {
    reply_with(
        "implemented",
        json!({ "variables": { "test_baseline": "all relevant checks passed before changes" } }),
    )
}

fn documented() -> Value {
    reply_with(
        "documented",
        json!({ "artifacts": { "learning_doc": LEARNING_DOC } }),
    )
}

/// The agent calls of the requirements stage, with the feedback checked:
/// brainstorm, doc_review, publish_requirements, check_requirements_feedback.
fn requirements_stage() -> Vec<Value> {
    vec![
        requirements_drafted(),
        reviewed(),
        published(),
        reply("no_feedback"),
    ]
}

/// The agent calls of the plan stage, with the feedback checked: plan,
/// plan_review, publish_plan, check_plan_feedback.
fn plan_stage() -> Vec<Value> {
    vec![plan_ready(), reviewed(), published(), reply("no_feedback")]
}

/// The agent calls of the work stage, with the feedback checked: work,
/// simplify, code_review, publish_work, check_work_feedback. `verify_work` is
/// a script and skips: the temporary workspace has no `scripts/verify.sh`.
fn work_stage() -> Vec<Value> {
    vec![
        implemented(),
        reply("simplified"),
        reply("clean"),
        published(),
        reply("no_feedback"),
    ]
}

/// Every agent call from the start up to `compound`, with every feedback
/// checked.
fn replies_up_to_compound() -> Vec<Value> {
    [requirements_stage(), plan_stage(), work_stage()].concat()
}

fn requirements_gates() -> Vec<Answer> {
    vec![Answer::new(
        "review_requirements",
        &[("requirements_decision", "check_feedback")],
    )]
}

fn plan_gates() -> Vec<Answer> {
    vec![Answer::new(
        "review_plan",
        &[("plan_decision", "check_feedback")],
    )]
}

/// Every gate from the start up to `compound`, each one with the feedback
/// checked.
fn gates_up_to_compound() -> Vec<Answer> {
    [
        requirements_gates(),
        plan_gates(),
        vec![Answer::new(
            "review_work",
            &[("work_decision", "check_feedback")],
        )],
    ]
    .concat()
}

/// One run of the shipped compound pipeline against the fake harness.
struct CompoundRun {
    root: TempRoot,
    harness_dir: PathBuf,
    engine: Engine,
    definition: WorkflowDefinition,
}

impl CompoundRun {
    /// `replies` answers the agent calls in order, `finalize` among them.
    fn new(replies: &[Value]) -> Self {
        let root = TempRoot::new();
        let harness_dir = root.dir("harness");
        let harness = FakeHarness::answering_in(harness_dir.clone(), replies);
        let engine = harness.engine(root.dir("store"));

        Self {
            root,
            harness_dir,
            engine,
            definition: shipped("compound-engineering.yaml"),
        }
    }

    /// Shortens the time limit of the `finalize` step itself.
    fn with_finalize_timeout(mut self, seconds: u64) -> Self {
        self.definition
            .states
            .get_mut("finalize")
            .expect("finalize exists")
            .timeout_secs = Some(seconds);
        self
    }

    /// Makes the n-th agent call sleep after it answers, past a time limit.
    fn with_call_hanging(self, call: usize, seconds: u32) -> Self {
        fs::write(
            self.harness_dir.join(format!("sleep.{call}")),
            seconds.to_string(),
        )
        .expect("write the sleep file");
        self
    }

    async fn run(&self, gates: &[Answer]) -> narsor_engine::domain::WorkflowExecution {
        run_to_terminal(
            &self.engine,
            &self.definition,
            json!({
                "feature_description": "let a reviewer subscribe to a workflow run",
                "target_repo": "acme/web",
            }),
            self.root.workspace(),
            gates,
        )
        .await
    }

    /// The prompt of the n-th agent call.
    fn prompt(&self, call: usize) -> String {
        fs::read_to_string(self.harness_dir.join(format!("args.{call}")))
            .unwrap_or_else(|error| panic!("read the prompt of call {call}: {error}"))
    }

    /// Every transition taken, as `(from, label, to)`.
    fn transitions(&self, execution_id: &str) -> Vec<(String, String, String)> {
        narsor_engine::persistence::ExecutionStore::new(self.root.dir("store"))
            .event_log(execution_id)
            .read_all()
            .expect("read the event log")
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::TransitionTaken {
                    from_state,
                    to_state,
                    output_label,
                } => Some((from_state, output_label, to_state)),
                _ => None,
            })
            .collect()
    }

    fn took(&self, execution_id: &str, from: &str, label: &str, to: &str) -> bool {
        self.transitions(execution_id)
            .iter()
            .any(|(f, l, t)| f == from && l == label && t == to)
    }

    /// The reason given with every completed action that chose `label`.
    fn reasons_for(&self, execution_id: &str, label: &str) -> Vec<String> {
        narsor_engine::persistence::ExecutionStore::new(self.root.dir("store"))
            .event_log(execution_id)
            .read_all()
            .expect("read the event log")
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::ActionCompleted {
                    output_label,
                    reason,
                    ..
                } if output_label == label => reason,
                _ => None,
            })
            .collect()
    }

    /// Whether the run ever entered `state`.
    fn visited(&self, execution_id: &str, state: &str) -> bool {
        self.transitions(execution_id)
            .iter()
            .any(|(_, _, to)| to == state)
    }

    /// The gate the run waits at, with the request it shows.
    fn pending(
        &self,
        execution_id: &str,
    ) -> narsor_engine::engine::actions::human::PendingHumanInput {
        self.engine
            .pending_input(execution_id)
            .expect("read the pending request")
            .expect("a request is pending")
    }
}

/// Covers AE4: every gate checks the feedback, and finalize takes the pull
/// request out of Draft.
#[tokio::test]
async fn covers_ae4_the_compound_engineering_pipeline_runs_through_all_of_its_gates_to_a_ready_request(
) {
    let replies = [
        replies_up_to_compound(),
        vec![
            documented(),
            published(),
            reply("no_feedback"),
            reply("ready"),
        ],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "review_learning",
            &[("learning_decision", "check_feedback")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
    assert!(run.took(
        &execution.id,
        "check_learning_feedback",
        "no_feedback",
        "finalize"
    ));
    assert!(run.took(&execution.id, "finalize", "ready", "finished"));
    assert_eq!(
        execution.context.variables.get("pr_url"),
        Some(&json!(PR_URL)),
        "the display reads the pull request from the context"
    );
    // The values the agents returned are rendered into the later prompts.
    assert!(
        run.prompt(17).contains(PR_URL),
        "finalize names the pull request"
    );
    assert!(
        run.prompt(2).contains(REQUIREMENTS_DOC),
        "doc_review names the requirements"
    );
    assert!(
        run.prompt(5).contains(REQUIREMENTS_DOC),
        "plan names the requirements"
    );
    assert!(
        run.prompt(6).contains(PLAN_DOC),
        "plan_review names the plan"
    );
    assert!(run.prompt(9).contains(PLAN_DOC), "work names the plan");
    assert!(
        run.prompt(12).contains(REQUIREMENTS_DOC) && run.prompt(12).contains(PLAN_DOC),
        "publish_work names the requirements and the plan"
    );
}

/// Covers AE1: `accept` at the work gate goes straight to `compound`.
#[tokio::test]
async fn covers_ae1_accept_at_the_work_gate_skips_the_feedback_check() {
    let replies = [
        requirements_stage(),
        plan_stage(),
        // work, simplify, code_review, publish_work; no check_work_feedback
        vec![
            implemented(),
            reply("simplified"),
            reply("clean"),
            published(),
        ],
        vec![reply("nothing_to_document"), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        requirements_gates(),
        plan_gates(),
        vec![Answer::new("review_work", &[("work_decision", "accept")])],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "review_work", "accept", "compound"));
    assert!(!run.visited(&execution.id, "check_work_feedback"));
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
}

/// Covers AE2: `check_feedback` at the requirements gate runs the feedback
/// check, and a thread that needs the person comes back to the same gate.
#[tokio::test]
async fn covers_ae2_check_feedback_runs_the_check_and_needs_human_returns_to_the_gate() {
    let replies = [
        requirements_drafted(),
        reviewed(),
        published(),
        reply("needs_human"),
    ];
    let run = CompoundRun::new(&replies);

    let execution = run.run(&requirements_gates()).await;

    assert!(run.took(
        &execution.id,
        "review_requirements",
        "check_feedback",
        "check_requirements_feedback"
    ));
    assert!(run.took(
        &execution.id,
        "check_requirements_feedback",
        "needs_human",
        "review_requirements"
    ));
    assert_eq!(run.pending(&execution.id).state_id, "review_requirements");
}

/// Covers AE3: `accept` at the learning gate goes straight to `finalize`.
#[tokio::test]
async fn covers_ae3_accept_at_the_learning_gate_goes_to_finalize() {
    let replies = [
        replies_up_to_compound(),
        vec![documented(), published(), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "review_learning",
            &[("learning_decision", "accept")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "review_learning", "accept", "finalize"));
    assert!(!run.visited(&execution.id, "check_learning_feedback"));
    assert!(run.took(&execution.id, "finalize", "ready", "finished"));
}

/// Covers AE8.
#[tokio::test]
async fn covers_ae8_nothing_to_document_goes_to_finalize_not_to_finished() {
    let replies = [
        replies_up_to_compound(),
        vec![reply("nothing_to_document"), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "compound", "nothing_to_document", "finalize"));
    assert!(run.took(&execution.id, "finalize", "ready", "finished"));
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// A blocker travels with the process: `work` hands its blocker to `plan`,
/// `plan` hands its own to `brainstorm`, and each step that read a blocker
/// clears it — so the third pass of `plan`, after the requirements came back
/// reviewed, no longer sees the stale one.
#[tokio::test]
async fn blockers_reach_the_author_of_the_stage_and_do_not_come_back() {
    let plan_blocker = "the plan names a table that does not exist";
    let requirements_blocker = "the requirements never say who may subscribe";
    let replies = [
        requirements_stage(), // calls 1–4
        plan_stage(),         // calls 5–8
        vec![
            // 9. work
            reply_with(
                "needs_plan_revision",
                json!({ "variables": { "plan_blocker": plan_blocker } }),
            ),
            // 10. plan, second entry
            reply_with(
                "needs_requirements_revision",
                json!({ "variables": {
                    "requirements_blocker": requirements_blocker,
                    "plan_blocker": "",
                } }),
            ),
        ],
        requirements_stage(), // calls 11–14
        // 15. plan, third entry. It sends the requirements back once more, so
        // the run stops at `brainstorm`, which has no answer and no retry
        // policy to wait out.
        vec![reply_with(
            "needs_requirements_revision",
            json!({ "variables": {
                "requirements_blocker": "still unclear",
                "plan_blocker": "",
            } }),
        )],
    ]
    .concat();
    let run = CompoundRun::new(&replies);

    let execution = run
        .run(&[requirements_gates(), plan_gates(), requirements_gates()].concat())
        .await;

    assert!(run.took(&execution.id, "work", "needs_plan_revision", "plan"));
    assert!(run.took(
        &execution.id,
        "plan",
        "needs_requirements_revision",
        "brainstorm"
    ));
    assert!(
        run.prompt(10).contains(plan_blocker),
        "plan reads the blocker of work"
    );
    assert!(
        run.prompt(11).contains(requirements_blocker),
        "brainstorm reads the blocker of plan"
    );
    assert!(
        !run.prompt(15).contains(plan_blocker),
        "the third plan pass must not see the blocker it already handled"
    );
}

#[tokio::test]
async fn revise_requirements_at_the_plan_gate_resumes_the_brainstorm_session() {
    let comment = "the plan shows the requirements miss the unsubscribe case";
    let replies = [
        requirements_stage(),
        vec![plan_ready(), reviewed(), published()],
        // 8. brainstorm, resumed: no answer, the run stops there
    ]
    .concat();
    let run = CompoundRun::new(&replies);

    let execution = run
        .run(
            &[
                requirements_gates(),
                vec![Answer::new(
                    "review_plan",
                    &[
                        ("plan_decision", "revise_requirements"),
                        ("plan_feedback", comment),
                    ],
                )],
            ]
            .concat(),
        )
        .await;

    assert!(run.took(
        &execution.id,
        "review_plan",
        "revise_requirements",
        "brainstorm"
    ));
    let resumed = run.prompt(8);
    assert!(
        resumed.contains("--resume\n--%ARG%--\nfake-session-1\n"),
        "brainstorm continues its own session: {resumed}"
    );
    assert!(
        resumed.contains(comment),
        "the comment is the message: {resumed}"
    );
}

/// The agent may skip the requirements review: the document goes straight to
/// publication and the reason travels in `review_summary`.
#[tokio::test]
async fn a_skipped_requirements_review_goes_straight_to_publication() {
    let ready = reply_with(
        "requirements_ready",
        json!({
            "artifacts": { "requirements_doc": REQUIREMENTS_DOC },
            "variables": { "review_summary": "Przegląd pominięto: zmiana jednej etykiety" },
        }),
    );
    // brainstorm, publish_requirements; the run stops at the feedback check.
    let replies = [ready, published()];
    let run = CompoundRun::new(&replies);

    let execution = run.run(&requirements_gates()).await;

    assert!(run.took(
        &execution.id,
        "brainstorm",
        "requirements_ready",
        "publish_requirements"
    ));
    assert!(run.took(
        &execution.id,
        "publish_requirements",
        "published",
        "review_requirements"
    ));
    assert!(
        run.prompt(2)
            .contains("Przegląd pominięto: zmiana jednej etykiety"),
        "the publish step reads the reason for the skipped review"
    );
}

#[tokio::test]
async fn dropping_the_learning_removes_it_and_goes_on_to_finalize() {
    let replies = [
        replies_up_to_compound(),
        vec![documented(), published(), reply("dropped"), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "review_learning",
            &[("learning_decision", "drop")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "review_learning", "drop", "drop_learning"));
    assert!(run.took(&execution.id, "drop_learning", "dropped", "finalize"));
    assert!(
        run.prompt(16).contains(LEARNING_DOC),
        "drop_learning names the document to remove"
    );
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
}

/// The agent calls up to `finalize`, which is call 15 when `compound` finds
/// nothing to document.
fn replies_up_to_finalize() -> Vec<Value> {
    [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat()
}

/// Covers AE2 and AE4: a finalize that cannot take the pull request out of
/// Draft stops at the gate with its reason. The run does not fail yet.
#[tokio::test]
async fn covers_ae4_a_blocked_finalize_stops_at_the_gate_with_its_reason() {
    let blocked = reply_with(
        "blocked",
        json!({ "reason": "glab is not signed in where the step runs" }),
    );
    let replies = [replies_up_to_finalize(), vec![blocked]].concat();
    let run = CompoundRun::new(&replies);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "finalize", "blocked", "finalize_blocked"));
    assert_ne!(execution.status, ExecutionStatus::Failed);
    let pending = run.pending(&execution.id);
    assert_eq!(pending.state_id, "finalize_blocked");
    let reason = pending.request.reason.unwrap_or_default();
    assert!(
        reason.contains("glab is not signed in"),
        "the gate shows why finalize stopped: {reason:?}"
    );
}

/// Covers AE4: `retry` runs finalize again.
#[tokio::test]
async fn covers_ae4_a_retried_finalize_can_end_ready() {
    let replies = [
        replies_up_to_finalize(),
        vec![reply("blocked"), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "finalize_blocked",
            &[("finalize_decision", "retry")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "finalize_blocked", "retry", "finalize"));
    assert!(run.took(&execution.id, "finalize", "ready", "finished"));
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// `retry` has no limit: two in a row neither fail the run nor stop it.
#[tokio::test]
async fn retrying_a_blocked_finalize_twice_is_not_limited() {
    let replies = [
        replies_up_to_finalize(),
        vec![reply("blocked"), reply("blocked"), reply("ready")],
    ]
    .concat();
    let run = CompoundRun::new(&replies);
    let retry = || Answer::new("finalize_blocked", &[("finalize_decision", "retry")]);
    let gates = [gates_up_to_compound(), vec![retry(), retry()]].concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "finalize", "ready", "finished"));
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// Covers AE5: `confirm_failure` ends the run failed.
#[tokio::test]
async fn covers_ae5_confirming_the_failure_ends_the_run_failed() {
    let replies = [replies_up_to_finalize(), vec![reply("blocked")]].concat();
    let run = CompoundRun::new(&replies);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "finalize_blocked",
            &[("finalize_decision", "confirm_failure")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(
        &execution.id,
        "finalize_blocked",
        "confirm_failure",
        "finalize_failed"
    ));
    assert_eq!(execution.current_state.as_deref(), Some("finalize_failed"));
    assert_eq!(execution.status, ExecutionStatus::Failed);
}

/// The engine stops a `finalize` that outlives its step limit and hands it
/// the declared `timeout` choice, so the run stops at the gate.
#[tokio::test]
async fn a_finalize_stopped_by_its_time_limit_stops_at_the_gate() {
    let replies = [replies_up_to_finalize(), vec![reply("ready")]].concat();
    let run = CompoundRun::new(&replies)
        .with_call_hanging(15, 30)
        .with_finalize_timeout(3);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "finalize", "timeout", "finalize_blocked"));
    assert_eq!(run.pending(&execution.id).state_id, "finalize_blocked");
}

/// `finalize` only reads the pull request on the host and changes its Draft
/// state: it needs no input of its own.
#[test]
fn finalize_reads_no_input() {
    let definition = shipped("compound-engineering.yaml");
    let state = &definition.states["finalize"];
    let prompt = built_prompt(state);
    assert!(!prompt.contains("inputs."), "finalize reads an input");
    let Some(ActionConfig::Agent(_)) = &state.action else {
        panic!("finalize is an agent step");
    };
}

/// The four review gates of the compound pipeline: a gate sends the stage back
/// with a comment, or goes forward with an optional note.
const COMPOUND_GATES: [&str; 4] = [
    "review_requirements",
    "review_plan",
    "review_work",
    "review_learning",
];

#[test]
fn every_review_gate_requires_a_comment_only_to_send_the_stage_back() {
    let definition = shipped("compound-engineering.yaml");
    for id in COMPOUND_GATES {
        let state = &definition.states[id];
        for choice in &state.choices {
            let sends_back = choice.label.starts_with("revise");
            assert_eq!(
                choice.resume_comment_field.is_some(),
                sends_back,
                "gate '{id}', choice '{}'",
                choice.label
            );
        }
        for label in ["check_feedback", "accept"] {
            assert!(
                state.choices.iter().any(|choice| choice.label == label),
                "gate '{id}' has no '{label}' choice"
            );
        }
    }
}

/// The graph of `workflows/compound-engineering.yaml` as the plan draws it:
/// every state, and every `(state, label, target)` edge.
const COMPOUND_STATES: [&str; 30] = [
    "brainstorm",
    "doc_review",
    "publish_requirements",
    "review_requirements",
    "check_requirements_feedback",
    "plan",
    "plan_review",
    "publish_plan",
    "review_plan",
    "check_plan_feedback",
    "work",
    "simplify",
    "code_review",
    "fix_findings",
    "verify_work",
    "repair_verification",
    "publish_work",
    "review_work",
    "work_revision",
    "check_work_feedback",
    "compound",
    "publish_learning",
    "review_learning",
    "drop_learning",
    "check_learning_feedback",
    "finalize",
    "finalize_blocked",
    "finished",
    "cancelled",
    "finalize_failed",
];

const COMPOUND_EDGES: [(&str, &str, &str); 62] = [
    ("brainstorm", "requirements_drafted", "doc_review"),
    ("brainstorm", "requirements_ready", "publish_requirements"),
    ("doc_review", "reviewed", "publish_requirements"),
    ("publish_requirements", "published", "review_requirements"),
    (
        "review_requirements",
        "check_feedback",
        "check_requirements_feedback",
    ),
    ("review_requirements", "accept", "plan"),
    ("review_requirements", "revise", "brainstorm"),
    ("review_requirements", "abandon", "cancelled"),
    ("check_requirements_feedback", "no_feedback", "plan"),
    (
        "check_requirements_feedback",
        "fixed_minor",
        "publish_requirements",
    ),
    ("check_requirements_feedback", "fixed_major", "doc_review"),
    (
        "check_requirements_feedback",
        "needs_human",
        "review_requirements",
    ),
    ("plan", "plan_ready", "plan_review"),
    ("plan", "needs_requirements_revision", "brainstorm"),
    ("plan_review", "reviewed", "publish_plan"),
    ("publish_plan", "published", "review_plan"),
    ("review_plan", "check_feedback", "check_plan_feedback"),
    ("review_plan", "accept", "work"),
    ("review_plan", "revise", "plan"),
    ("review_plan", "revise_requirements", "brainstorm"),
    ("review_plan", "abandon", "cancelled"),
    ("check_plan_feedback", "no_feedback", "work"),
    ("check_plan_feedback", "fixed_minor", "publish_plan"),
    ("check_plan_feedback", "fixed_major", "plan_review"),
    ("check_plan_feedback", "needs_human", "review_plan"),
    ("work", "implemented", "simplify"),
    ("work", "needs_plan_revision", "plan"),
    ("simplify", "simplified", "code_review"),
    ("simplify", "unchanged", "code_review"),
    ("code_review", "clean", "verify_work"),
    ("code_review", "findings", "fix_findings"),
    ("fix_findings", "fixed", "code_review"),
    ("verify_work", "passed", "publish_work"),
    ("verify_work", "skipped", "publish_work"),
    ("verify_work", "failed", "repair_verification"),
    ("repair_verification", "repaired", "code_review"),
    ("publish_work", "published", "review_work"),
    ("review_work", "check_feedback", "check_work_feedback"),
    ("review_work", "accept", "compound"),
    ("review_work", "revise", "work_revision"),
    ("review_work", "revise_plan", "plan"),
    ("review_work", "abandon", "cancelled"),
    ("work_revision", "revised", "code_review"),
    ("check_work_feedback", "no_feedback", "compound"),
    ("check_work_feedback", "fixed", "code_review"),
    ("check_work_feedback", "needs_human", "review_work"),
    ("compound", "documented", "publish_learning"),
    ("compound", "nothing_to_document", "finalize"),
    ("publish_learning", "published", "review_learning"),
    (
        "review_learning",
        "check_feedback",
        "check_learning_feedback",
    ),
    ("review_learning", "accept", "finalize"),
    ("review_learning", "revise", "compound"),
    ("review_learning", "drop", "drop_learning"),
    ("drop_learning", "dropped", "finalize"),
    ("check_learning_feedback", "no_feedback", "finalize"),
    ("check_learning_feedback", "fixed", "publish_learning"),
    ("check_learning_feedback", "needs_human", "review_learning"),
    ("finalize", "ready", "finished"),
    ("finalize", "blocked", "finalize_blocked"),
    ("finalize", "timeout", "finalize_blocked"),
    ("finalize_blocked", "retry", "finalize"),
    ("finalize_blocked", "confirm_failure", "finalize_failed"),
];

#[test]
fn the_compound_graph_is_the_one_the_plan_draws() {
    let definition = shipped("compound-engineering.yaml");

    let mut states: Vec<&str> = definition.states.keys().map(String::as_str).collect();
    states.sort_unstable();
    let mut expected_states = COMPOUND_STATES.to_vec();
    expected_states.sort_unstable();
    assert_eq!(states, expected_states);

    assert_eq!(edges(&definition), sorted(&COMPOUND_EDGES));
    assert_eq!(
        definition.states["finalize_failed"].terminal_status,
        Some(TerminalStatus::Failed),
        "a failure confirmed at the finalize gate failed the run"
    );
}

#[tokio::test]
async fn the_open_mercato_pipeline_runs_through_its_review_loop_and_merge_gates() {
    let root = TempRoot::new();
    let pr_url = "https://example.test/open-mercato/app/pull/42";
    // Every agent call of the path below, in order, in the reply format the
    // engine's contract asks for. Informational fields go to
    // `additional_data`; only the PR, which the display shows, is a variable.
    let reply = |choice: &str, additional_data: Value| {
        json!({
            "choice": choice,
            "reason": format!("{choice}: the step's work is done"),
            "additional_data": additional_data,
        })
    };
    let harness = FakeHarness::answering_in(
        root.dir("harness"),
        &[
            reply("routed", json!({ "brief": null })), // discover
            reply("ready", json!({ "ready_status": "READY", "gaps": [] })), // intake
            reply(
                "actionable",
                json!({ "evidence": "empty list renders blank" }),
            ), // triage
            reply("claimed", json!({ "claimed_by": "narsor" })), // claim
            json!({
                "choice": "user_facing",
                "reason": "the orders list is a screen a merchant sees",
            }), // assess_surface
            reply("shaped", json!({ "states_covered": ["empty"] })), // shape_design
            reply("implemented", json!({ "files_changed": 2 })), // implement
            json!({
                "choice": "pr_opened",
                "reason": "the pull request is open and labelled",
                "variables": { "pr_number": "42", "pr_url": pr_url },
            }), // open_pr
            reply("changes_requested", json!({ "findings": 1, "blockers": 0 })), // code_review
            reply("updated", json!({ "threads_addressed": 1 })), // address_feedback
            reply("approved", json!({ "findings": 0, "blockers": 0 })), // code_review again
            reply("needs_qa", json!({ "qa_label": "needs-qa" })), // qa_routing
            reply(
                "evidence_ready",
                json!({ "scenarios": 7, "qa_head": "deadbeef" }),
            ), // qa_run
            reply("mergeable", json!({ "blockers": [] })), // premerge_check
            reply("merged", json!({ "merge_commit": "c0ffee" })), // merge_pr
            reply("reconciled", json!({ "closed": 1, "follow_ups": 0 })), // housekeeping
        ],
    );
    let engine = harness.engine(root.dir("store"));
    let definition = shipped("open-mercato-sdlc.yaml");

    // The validation gate runs a real script — a two-line fixture in the
    // temporary root, never the repository's own.
    let gate = root.dir("scripts").join("validation-gate.sh");
    fs::write(&gate, "#!/bin/sh\nexit 0\n").expect("write the validation gate fixture");

    let execution = run_to_terminal(
        &engine,
        &definition,
        json!({
            "ticket_id": "OM-417",
            "task_brief": "the empty state of the orders list says nothing",
            "target_repo": "open-mercato/app",
            "validation_command": gate.to_string_lossy(),
        }),
        root.workspace(),
        &[
            Answer::new(
                "confirm_readiness",
                &[
                    ("readiness_decision", "confirmed"),
                    (
                        "readiness_notes",
                        "merchants with no orders yet; out of scope: filters",
                    ),
                ],
            ),
            Answer::new("approve_design", &[("design_decision", "approved")]),
            Answer::new(
                "qa_signoff",
                &[("qa_decision", "qa_approved"), ("qa_head", "deadbeef")],
            ),
            Answer::new("approve_merge", &[("merge_decision", "merge")]),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
    assert_eq!(
        execution.context.variables.get("qa_head"),
        Some(&json!("deadbeef")),
        "the QA head the reviewer pinned the approval to is kept in the context"
    );
    // `open_pr` declares the PR, so it reaches the context the display reads
    // (`{{ context.variables.pr_number }}`, `{{ context.variables.pr_url }}`).
    assert_eq!(
        execution.context.variables.get("pr_number"),
        Some(&json!("42"))
    );
    assert_eq!(
        execution.context.variables.get("pr_url"),
        Some(&json!(pr_url))
    );
    // Informational fields stay out of the context (KTD5).
    for informational in ["brief", "evidence", "findings", "merge_commit"] {
        assert!(
            execution.context.variables.get(informational).is_none(),
            "'{informational}' is additional data, not a variable"
        );
    }
}

/// The graph of the Open Mercato pipeline before its prompts moved to the
/// reply contract: every state, and every `(state, label, target)` edge.
/// Moving to the new contract changes how agents answer, never where the
/// process goes (R23).
const OPEN_MERCATO_STATES: [&str; 23] = [
    "discover",
    "intake",
    "confirm_readiness",
    "triage",
    "claim",
    "assess_surface",
    "shape_design",
    "approve_design",
    "implement",
    "validation_gate",
    "open_pr",
    "code_review",
    "address_feedback",
    "qa_routing",
    "qa_run",
    "qa_signoff",
    "premerge_check",
    "approve_merge",
    "merge_pr",
    "housekeeping",
    "finished",
    "closed_no_action",
    "cancelled",
];

const OPEN_MERCATO_EDGES: [(&str, &str, &str); 38] = [
    ("discover", "routed", "intake"),
    ("discover", "no_action", "closed_no_action"),
    ("intake", "ready", "confirm_readiness"),
    ("intake", "gaps_found", "confirm_readiness"),
    ("confirm_readiness", "confirmed", "triage"),
    ("confirm_readiness", "waived", "triage"),
    ("confirm_readiness", "not_ready", "intake"),
    ("confirm_readiness", "abandon", "cancelled"),
    ("triage", "actionable", "claim"),
    ("triage", "no_action", "closed_no_action"),
    ("triage", "already_claimed", "closed_no_action"),
    ("claim", "claimed", "assess_surface"),
    ("claim", "claim_lost", "closed_no_action"),
    ("assess_surface", "user_facing", "shape_design"),
    ("assess_surface", "not_user_facing", "implement"),
    ("shape_design", "shaped", "approve_design"),
    ("approve_design", "approved", "implement"),
    ("approve_design", "reshape", "shape_design"),
    ("approve_design", "abandon", "cancelled"),
    ("implement", "implemented", "validation_gate"),
    ("validation_gate", "green", "open_pr"),
    ("validation_gate", "red", "implement"),
    ("open_pr", "pr_opened", "code_review"),
    ("code_review", "approved", "qa_routing"),
    ("code_review", "changes_requested", "address_feedback"),
    ("address_feedback", "updated", "code_review"),
    ("qa_routing", "needs_qa", "qa_run"),
    ("qa_routing", "skip_qa", "premerge_check"),
    ("qa_run", "evidence_ready", "qa_signoff"),
    ("qa_signoff", "qa_approved", "premerge_check"),
    ("qa_signoff", "qa_failed", "implement"),
    ("qa_signoff", "abandon", "cancelled"),
    ("premerge_check", "mergeable", "approve_merge"),
    ("approve_merge", "merge", "merge_pr"),
    ("approve_merge", "hold", "approve_merge"),
    ("approve_merge", "abandon", "cancelled"),
    ("merge_pr", "merged", "housekeeping"),
    ("housekeeping", "reconciled", "finished"),
];

#[test]
fn the_open_mercato_graph_keeps_its_states_and_transitions() {
    let definition = shipped("open-mercato-sdlc.yaml");

    let mut states: Vec<&str> = definition.states.keys().map(String::as_str).collect();
    states.sort_unstable();
    let mut expected_states = OPEN_MERCATO_STATES.to_vec();
    expected_states.sort_unstable();
    assert_eq!(states, expected_states);

    assert_eq!(edges(&definition), sorted(&OPEN_MERCATO_EDGES));

    // No choice gained a requirement: the gates decide as they did before.
    for (id, state) in &definition.states {
        for choice in &state.choices {
            assert!(
                choice.requires_variables.is_empty() && choice.requires_artifacts.is_empty(),
                "state '{id}' choice '{}' gained a requirement",
                choice.label
            );
        }
    }
}

/// The QA gate really does route a failure back into implementation, and the
/// merge gate can stay paused in place and later be abandoned — the two loops
/// SDLC.md insists on.
#[tokio::test]
async fn a_failed_qa_goes_back_to_implementation_and_a_held_merge_can_be_abandoned() {
    let root = TempRoot::new();
    let harness = FakeHarness::installed_in(
        root.dir("harness"),
        &[
            "routed",          // discover
            "ready",           // intake
            "actionable",      // triage
            "claimed",         // claim
            "not_user_facing", // assess_surface — the Design stage is skipped
            "implemented",     // implement
            "pr_opened",       // open_pr
            "approved",        // code_review
            "needs_qa",        // qa_routing
            "evidence_ready",  // qa_run
            "implemented",     // implement, after QA failed
            "pr_opened",       // open_pr again
            "approved",        // code_review again
            "needs_qa",        // qa_routing again
            "evidence_ready",  // qa_run again
            "mergeable",       // premerge_check
        ],
    );
    let engine = harness.engine(root.dir("store"));
    let definition = shipped("open-mercato-sdlc.yaml");

    let gate = root.dir("scripts").join("validation-gate.sh");
    fs::write(&gate, "#!/bin/sh\nexit 0\n").expect("write the validation gate fixture");

    let execution = run_to_terminal(
        &engine,
        &definition,
        json!({
            "ticket_id": "OM-418",
            "task_brief": "orders export times out",
            "target_repo": "open-mercato/app",
            "validation_command": gate.to_string_lossy(),
        }),
        root.workspace(),
        &[
            Answer::new(
                "confirm_readiness",
                &[
                    ("readiness_decision", "waived"),
                    (
                        "readiness_notes",
                        "maintainer waives the out-of-scope section",
                    ),
                ],
            ),
            Answer::new(
                "qa_signoff",
                &[("qa_decision", "qa_failed"), ("qa_head", "cafe1234")],
            ),
            Answer::new(
                "qa_signoff",
                &[("qa_decision", "qa_approved"), ("qa_head", "cafe5678")],
            ),
            Answer::new("approve_merge", &[("merge_decision", "hold")]),
            Answer::new("approve_merge", &[("merge_decision", "abandon")]),
        ],
    )
    .await;

    assert_eq!(
        execution.status,
        ExecutionStatus::Cancelled,
        "a human holding the merge and then abandoning cancels the run"
    );
    assert_eq!(execution.current_state.as_deref(), Some("cancelled"));
}
