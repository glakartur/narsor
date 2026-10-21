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
//! **No real harness and no real `git`/`glab` is ever run here.** The agent
//! states are driven by `tests/fixtures/harness/fake_harness.sh` through
//! [`ClaudeCodeAdapter::with_program`], the way `tests/actions_agent.rs` does
//! it. The script states run in a temporary workspace: `verify_work` finds no
//! `scripts/verify.sh` there, and `finalize` and `disarm_merge` find
//! `tests/fixtures/glab/fake_glab.sh` first on their `PATH` (it needs `jq`).
//! Nothing is ever written into the repository.

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
const SHIPPED: [(&str, &str); 2] = [
    ("compound-engineering", "compound-engineering.yaml"),
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

    fn approval(state: &'static str) -> Self {
        Self {
            state,
            values: Vec::new(),
            approve: true,
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
    format!("{}{}", config.prompt, output_contract(&StepContract::of(state)))
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

/// The merge request every publish step reports. `finalize` reads its number
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

/// The agent calls of the requirements stage, approved at once: brainstorm,
/// doc_review, publish_requirements, check_requirements_feedback.
fn requirements_stage() -> Vec<Value> {
    vec![
        requirements_drafted(),
        reviewed(),
        published(),
        reply("no_feedback"),
    ]
}

/// The agent calls of the plan stage, approved at once: plan, plan_review,
/// publish_plan, check_plan_feedback.
fn plan_stage() -> Vec<Value> {
    vec![plan_ready(), reviewed(), published(), reply("no_feedback")]
}

/// The agent calls of the work stage, approved at once: work, simplify,
/// code_review, publish_work, check_work_feedback. `verify_work` is a script
/// and skips: the temporary workspace has no `scripts/verify.sh`.
fn work_stage() -> Vec<Value> {
    vec![
        implemented(),
        reply("simplified"),
        reply("clean"),
        published(),
        reply("no_feedback"),
    ]
}

/// Every agent call from the start up to `compound`, on the shortest path.
fn replies_up_to_compound() -> Vec<Value> {
    [requirements_stage(), plan_stage(), work_stage()].concat()
}

fn requirements_gates() -> Vec<Answer> {
    vec![
        Answer::approval("doc_review"),
        Answer::new(
            "approve_requirements",
            &[("requirements_decision", "approve")],
        ),
    ]
}

fn plan_gates() -> Vec<Answer> {
    vec![
        Answer::approval("plan_review"),
        Answer::new("approve_plan", &[("plan_decision", "approve")]),
    ]
}

/// Every gate from the start up to `compound`, each one approved.
fn gates_up_to_compound() -> Vec<Answer> {
    [
        requirements_gates(),
        plan_gates(),
        vec![Answer::new("approve_work", &[("work_decision", "approve")])],
    ]
    .concat()
}

/// The merge request as `glab mr view -F json` prints it.
fn merge_request(state: &str, pipeline: &str) -> Value {
    json!({
        "iid": 7,
        "project_id": 42,
        "state": state,
        "draft": false,
        "target_branch": "develop",
        "has_conflicts": false,
        "detailed_merge_status": "ci_still_running",
        "merge_when_pipeline_succeeds": true,
        "head_pipeline": { "status": pipeline },
    })
}

fn open_merge_request() -> Value {
    merge_request("opened", "running")
}

fn merged_merge_request() -> Value {
    merge_request("merged", "success")
}

/// The fake `glab` of `tests/fixtures/glab/fake_glab.sh`, installed as `glab`
/// in a directory of its own.
struct FakeGlab {
    dir: PathBuf,
}

impl FakeGlab {
    /// Installs the fixture and has the n-th `glab mr view` print the n-th
    /// merge request, and the last one from then on.
    fn serving(dir: PathBuf, views: &[Value]) -> Self {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/glab/fake_glab.sh");
        let temp = dir.join(format!(".glab.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake glab");
        fs::rename(&temp, dir.join("glab")).expect("move the fake glab into place");

        for (index, view) in views.iter().enumerate() {
            fs::write(dir.join(format!("view.{}", index + 1)), view.to_string())
                .expect("write a merge request view");
        }
        let last = views.last().expect("at least one merge request view");
        fs::write(dir.join("view"), last.to_string()).expect("write the fallback view");

        Self { dir }
    }

    /// Makes `glab mr <command>` sleep before it answers.
    fn hanging_on(self, command: &str, seconds: u32) -> Self {
        fs::write(self.dir.join(format!("hang.{command}")), seconds.to_string())
            .expect("write the hang file");
        self
    }

    /// The `PATH` a step sees: this directory first.
    fn path(&self) -> String {
        format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    /// Every call so far, one line of arguments each.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// How many calls start with `prefix` (e.g. `mr approve`).
    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    /// Whether auto-merge was cancelled and the Draft flag restored.
    fn disarmed(&self) -> bool {
        let calls = self.calls();
        calls
            .iter()
            .any(|call| call.contains("cancel_merge_when_pipeline_succeeds"))
            && calls
                .iter()
                .any(|call| call.starts_with("mr update") && call.contains("--draft"))
    }
}

/// One run of the shipped compound pipeline against the fake harness and the
/// fake `glab`.
struct CompoundRun {
    root: TempRoot,
    harness_dir: PathBuf,
    glab: FakeGlab,
    engine: Engine,
    definition: WorkflowDefinition,
}

impl CompoundRun {
    /// `replies` answers the agent calls in order; `views` is what `glab`
    /// reports about the merge request. `finalize` waits for no result at all
    /// (`FINALIZE_WAIT_SECS=0`): it reads the merge request once after setting
    /// auto-merge, so every outcome is decided by `views`.
    fn new(replies: &[Value], views: &[Value]) -> Self {
        let root = TempRoot::new();
        let harness_dir = root.dir("harness");
        let harness = FakeHarness::answering_in(harness_dir.clone(), replies);
        let engine = harness.engine(root.dir("store"));
        let glab = FakeGlab::serving(root.dir("glab"), views);

        let mut definition = shipped("compound-engineering.yaml");
        for step in ["finalize", "disarm_merge"] {
            let env = &mut script_config(&mut definition, step).env;
            env.insert("PATH".to_string(), glab.path());
            env.insert("FINALIZE_WAIT_SECS".to_string(), "0".to_string());
            env.insert("FINALIZE_POLL_SECS".to_string(), "0".to_string());
        }

        Self {
            root,
            harness_dir,
            glab,
            engine,
            definition,
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

    fn with_glab(mut self, configure: impl FnOnce(FakeGlab) -> FakeGlab) -> Self {
        self.glab = configure(self.glab);
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

    /// The gate the run waits at, with the request it shows.
    fn pending(&self, execution_id: &str) -> narsor_engine::engine::actions::human::PendingHumanInput {
        self.engine
            .pending_input(execution_id)
            .expect("read the pending request")
            .expect("a request is pending")
    }
}

/// The script configuration of a step, to point it at the fake `glab`.
fn script_config<'a>(
    definition: &'a mut WorkflowDefinition,
    step: &str,
) -> &'a mut narsor_engine::domain::ScriptActionConfig {
    match definition.states.get_mut(step).map(|state| &mut state.action) {
        Some(Some(ActionConfig::Script(config))) => config,
        _ => panic!("'{step}' is a script step"),
    }
}

#[tokio::test]
async fn the_compound_engineering_pipeline_runs_through_all_of_its_gates_and_merges() {
    let replies = [
        replies_up_to_compound(),
        vec![documented(), published(), reply("no_feedback")],
    ]
    .concat();
    let run = CompoundRun::new(&replies, &[open_merge_request(), merged_merge_request()]);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new(
            "approve_learning",
            &[("learning_decision", "approve")],
        )],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
    assert!(run.took(&execution.id, "check_learning_feedback", "no_feedback", "finalize"));
    assert!(run.took(&execution.id, "finalize", "merged", "finished"));
    assert_eq!(
        execution.context.variables.get("pr_url"),
        Some(&json!(PR_URL)),
        "the display reads the merge request from the context"
    );
    // finalize takes the merge request out of Draft, approves it and sets
    // auto-merge — and, merged, leaves it armed.
    assert_eq!(run.glab.count("mr update 7"), 1, "{:?}", run.glab.calls());
    assert!(run.glab.calls().iter().any(|call| call.contains("--ready")));
    assert_eq!(run.glab.count("mr approve 7"), 1);
    assert_eq!(run.glab.count("mr merge 7"), 1);
    assert!(run
        .glab
        .calls()
        .iter()
        .any(|call| call.starts_with("mr merge") && call.contains("--auto-merge")));
    assert!(!run.glab.disarmed(), "a merged request is not disarmed");
    // The documents the agents returned are named in the later prompts.
    assert!(run.prompt(2).contains(REQUIREMENTS_DOC), "doc_review names the requirements");
    assert!(run.prompt(5).contains(REQUIREMENTS_DOC), "plan names the requirements");
    assert!(run.prompt(6).contains(PLAN_DOC), "plan_review names the plan");
    assert!(run.prompt(9).contains(PLAN_DOC), "work names the plan");
}

/// Covers AE8.
#[tokio::test]
async fn covers_ae8_nothing_to_document_goes_to_finalize_not_to_finished() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(&replies, &[open_merge_request(), merged_merge_request()]);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "compound", "nothing_to_document", "finalize"));
    assert!(run.took(&execution.id, "finalize", "merged", "finished"));
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// A blocker travels with the process: `work` hands its blocker to `plan`,
/// `plan` hands its own to `brainstorm`, and each step that read a blocker
/// clears it — so the third pass of `plan`, after the requirements came back
/// approved, no longer sees the stale one.
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
    let run = CompoundRun::new(&replies, &[open_merge_request()]);

    let execution = run
        .run(&[requirements_gates(), plan_gates(), requirements_gates()].concat())
        .await;

    assert!(run.took(&execution.id, "work", "needs_plan_revision", "plan"));
    assert!(run.took(&execution.id, "plan", "needs_requirements_revision", "brainstorm"));
    assert!(run.prompt(10).contains(plan_blocker), "plan reads the blocker of work");
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
    let run = CompoundRun::new(&replies, &[open_merge_request()]);

    let execution = run
        .run(
            &[
                requirements_gates(),
                vec![
                    Answer::approval("plan_review"),
                    Answer::new(
                        "approve_plan",
                        &[
                            ("plan_decision", "revise_requirements"),
                            ("plan_feedback", comment),
                        ],
                    ),
                ],
            ]
            .concat(),
        )
        .await;

    assert!(run.took(&execution.id, "approve_plan", "revise_requirements", "brainstorm"));
    let resumed = run.prompt(8);
    assert!(
        resumed.contains("--resume\n--%ARG%--\nfake-session-1\n"),
        "brainstorm continues its own session: {resumed}"
    );
    assert!(resumed.contains(comment), "the comment is the message: {resumed}");
}

#[tokio::test]
async fn dropping_the_learning_removes_it_and_goes_on_to_finalize() {
    let replies = [
        replies_up_to_compound(),
        vec![documented(), published(), reply("dropped")],
    ]
    .concat();
    let run = CompoundRun::new(&replies, &[open_merge_request(), merged_merge_request()]);
    let gates = [
        gates_up_to_compound(),
        vec![Answer::new("approve_learning", &[("learning_decision", "drop")])],
    ]
    .concat();

    let execution = run.run(&gates).await;

    assert!(run.took(&execution.id, "approve_learning", "drop", "drop_learning"));
    assert!(run.took(&execution.id, "drop_learning", "dropped", "finalize"));
    assert!(
        run.prompt(16).contains(LEARNING_DOC),
        "drop_learning names the document to remove"
    );
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
}

/// Covers AE7: a failed pipeline is a repair, and the merge is disarmed first.
#[tokio::test]
async fn covers_ae7_a_failed_pipeline_disarms_the_merge_and_goes_to_repair() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(
        &replies,
        &[
            open_merge_request(),
            merge_request("opened", "failed"),
            merge_request("opened", "failed"),
        ],
    );

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "finalize", "pipeline_failed", "repair_merge"));
    assert!(run.glab.disarmed(), "{:?}", run.glab.calls());
    assert!(
        run.prompt(15).contains("pipeline"),
        "repair_merge reads why finalize stopped"
    );
}

/// Covers AE7: no result within the limit is `blocked`, and the gate after
/// it shows the reason `finalize` gave.
#[tokio::test]
async fn covers_ae7_no_result_within_the_limit_stops_at_the_blocked_gate_with_its_reason() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(&replies, &[open_merge_request()]);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "finalize", "blocked", "finalize_blocked"));
    assert!(run.glab.disarmed(), "{:?}", run.glab.calls());
    let pending = run.pending(&execution.id);
    assert_eq!(pending.state_id, "finalize_blocked");
    let reason = pending.request.reason.unwrap_or_default();
    assert!(
        reason.contains("not merged within"),
        "the gate shows why finalize stopped: {reason:?}"
    );
}

#[tokio::test]
async fn a_retried_finalize_finds_the_request_already_merged() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(
        &replies,
        &[
            open_merge_request(),
            open_merge_request(),
            open_merge_request(),
            merged_merge_request(),
        ],
    );
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
    assert!(run.took(&execution.id, "finalize", "merged", "finished"));
    assert_eq!(
        run.glab.count("mr approve"),
        1,
        "a merged request is not approved again: {:?}",
        run.glab.calls()
    );
    assert_eq!(run.glab.count("mr merge"), 1);
}

#[tokio::test]
async fn a_request_that_merged_while_being_disarmed_is_merged() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(
        &replies,
        &[
            open_merge_request(),
            open_merge_request(),
            merged_merge_request(),
        ],
    );

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.glab.disarmed(), "{:?}", run.glab.calls());
    assert!(run.took(&execution.id, "finalize", "merged", "finished"));
    assert_eq!(execution.status, ExecutionStatus::Completed);
}

/// The engine kills a `finalize` that outlives its step limit, so its own
/// clean-up never runs: `disarm_merge` does it instead.
#[tokio::test]
async fn a_finalize_killed_by_its_time_limit_is_disarmed_by_its_own_step() {
    let replies = [replies_up_to_compound(), vec![reply("nothing_to_document")]].concat();
    let run = CompoundRun::new(&replies, &[open_merge_request()])
        .with_glab(|glab| glab.hanging_on("merge", 30))
        .with_finalize_timeout(3);

    let execution = run.run(&gates_up_to_compound()).await;

    assert!(run.took(&execution.id, "finalize", "timeout", "disarm_merge"));
    assert!(run.took(&execution.id, "disarm_merge", "disarmed", "finalize_blocked"));
    assert!(run.glab.disarmed(), "{:?}", run.glab.calls());
    assert_eq!(run.pending(&execution.id).state_id, "finalize_blocked");
}

/// The graph of `workflows/compound-engineering.yaml` as the plan draws it:
/// every state, and every `(state, label, target)` edge.
const COMPOUND_STATES: [&str; 32] = [
    "brainstorm",
    "doc_review",
    "publish_requirements",
    "approve_requirements",
    "check_requirements_feedback",
    "plan",
    "plan_review",
    "publish_plan",
    "approve_plan",
    "check_plan_feedback",
    "work",
    "simplify",
    "code_review",
    "fix_findings",
    "verify_work",
    "repair_verification",
    "publish_work",
    "approve_work",
    "work_revision",
    "check_work_feedback",
    "compound",
    "publish_learning",
    "approve_learning",
    "drop_learning",
    "check_learning_feedback",
    "finalize",
    "repair_merge",
    "disarm_merge",
    "finalize_blocked",
    "finished",
    "cancelled",
    "not_merged",
];

const COMPOUND_EDGES: [(&str, &str, &str); 62] = [
    ("brainstorm", "requirements_drafted", "doc_review"),
    ("doc_review", "reviewed", "publish_requirements"),
    ("publish_requirements", "published", "approve_requirements"),
    ("approve_requirements", "approve", "check_requirements_feedback"),
    ("approve_requirements", "revise", "brainstorm"),
    ("approve_requirements", "abandon", "cancelled"),
    ("check_requirements_feedback", "no_feedback", "plan"),
    ("check_requirements_feedback", "fixed_minor", "publish_requirements"),
    ("check_requirements_feedback", "fixed_major", "doc_review"),
    ("check_requirements_feedback", "needs_human", "approve_requirements"),
    ("plan", "plan_ready", "plan_review"),
    ("plan", "needs_requirements_revision", "brainstorm"),
    ("plan_review", "reviewed", "publish_plan"),
    ("publish_plan", "published", "approve_plan"),
    ("approve_plan", "approve", "check_plan_feedback"),
    ("approve_plan", "revise", "plan"),
    ("approve_plan", "revise_requirements", "brainstorm"),
    ("approve_plan", "abandon", "cancelled"),
    ("check_plan_feedback", "no_feedback", "work"),
    ("check_plan_feedback", "fixed_minor", "publish_plan"),
    ("check_plan_feedback", "fixed_major", "plan_review"),
    ("check_plan_feedback", "needs_human", "approve_plan"),
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
    ("publish_work", "published", "approve_work"),
    ("approve_work", "approve", "check_work_feedback"),
    ("approve_work", "revise", "work_revision"),
    ("approve_work", "revise_plan", "plan"),
    ("approve_work", "abandon", "cancelled"),
    ("work_revision", "revised", "code_review"),
    ("check_work_feedback", "no_feedback", "compound"),
    ("check_work_feedback", "fixed", "code_review"),
    ("check_work_feedback", "needs_human", "approve_work"),
    ("compound", "documented", "publish_learning"),
    ("compound", "nothing_to_document", "finalize"),
    ("publish_learning", "published", "approve_learning"),
    ("approve_learning", "approve", "check_learning_feedback"),
    ("approve_learning", "revise", "compound"),
    ("approve_learning", "drop", "drop_learning"),
    ("drop_learning", "dropped", "finalize"),
    ("check_learning_feedback", "no_feedback", "finalize"),
    ("check_learning_feedback", "fixed", "publish_learning"),
    ("check_learning_feedback", "needs_human", "approve_learning"),
    ("finalize", "merged", "finished"),
    ("finalize", "pipeline_failed", "repair_merge"),
    ("finalize", "conflict", "repair_merge"),
    ("finalize", "blocked", "finalize_blocked"),
    ("finalize", "timeout", "disarm_merge"),
    ("repair_merge", "repaired", "code_review"),
    ("disarm_merge", "merged", "finished"),
    ("disarm_merge", "disarmed", "finalize_blocked"),
    ("finalize_blocked", "retry", "finalize"),
    ("finalize_blocked", "finish_unmerged", "not_merged"),
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
        definition.states["not_merged"].terminal_status,
        Some(TerminalStatus::Cancelled),
        "work that never reached the target branch did not complete"
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
            reply("actionable", json!({ "evidence": "empty list renders blank" })), // triage
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
            reply("evidence_ready", json!({ "scenarios": 7, "qa_head": "deadbeef" })), // qa_run
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
