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
//! **No real harness and no real `git`/`gh` is ever run here.** The agent
//! states are driven by `tests/fixtures/harness/fake_harness.sh` through
//! [`ClaudeCodeAdapter::with_program`], the way `tests/actions_agent.rs` does
//! it, and the one script state is pointed at a two-line fixture written into
//! a temporary directory. Nothing is ever written into the repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use narsor_engine::domain::{
    ActionConfig, ExecutionStatus, HumanInputType, State, TerminalStatus, WorkflowDefinition,
    Workspace,
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

#[tokio::test]
async fn the_compound_engineering_pipeline_runs_through_all_of_its_gates() {
    let root = TempRoot::new();
    let harness_dir = root.dir("harness");
    let requirements_doc = "docs/brainstorms/2026-10-16-subscribe-requirements.md";
    let plan_doc = "docs/plans/2026-10-16-subscribe-plan.md";
    let label = |label: &str| json!({ "choice": label });
    // Every agent call of the path below, in order.
    let harness = FakeHarness::answering_in(
        harness_dir.clone(),
        &[
            // 1. brainstorm
            json!({
                "choice": "requirements_drafted",
                "variables": { "requirements_doc": requirements_doc },
            }),
            label("reviewed"), // 2. doc_review
            json!({ "choice": "published", "variables": { "pr_url": "https://example.test/pr/1" } }), // publish_scope
            label("no_feedback"), // resolve_scope_feedback
            label("no_feedback"), // confirm_scope_feedback after approval
            // 6. plan
            json!({ "choice": "plan_ready", "variables": { "plan_doc": plan_doc } }),
            label("updated"),     // publish_plan
            label("no_feedback"), // resolve_plan_feedback
            label("no_feedback"), // confirm_plan_feedback after approval
            json!({
                "choice": "implemented",
                "variables": { "test_baseline": "all relevant checks passed before changes" },
            }), // 10. work — ambiguity uses awaiting_input in place
            label("simplified"),  // simplify
            label("clean"),       // code_review
            json!({ "choice": "published", "variables": { "pr_url": "https://example.test/pr/1" } }), // publish_work
            label("no_feedback"), // resolve_work_feedback
            label("revised"),     // work_revision — addresses human review
            label("simplified"),  // simplify
            label("clean"),       // code_review
            json!({ "choice": "published", "variables": { "pr_url": "https://example.test/pr/1" } }), // publish_work
            label("fixed"),       // resolve_work_feedback — fixes are already pushed
            label("no_feedback"), // confirm_work_feedback after approval
            label("documented"),  // compound — after implementation review
            label("updated"),     // publish_learning — push the knowledge doc
            label("no_feedback"), // resolve_learning_feedback
            label("no_feedback"), // confirm_learning_feedback after approval
        ],
    );
    let engine = harness.engine(root.dir("store"));
    let definition = shipped("compound-engineering.yaml");

    let execution = run_to_terminal(
        &engine,
        &definition,
        json!({
            "feature_description": "let a reviewer subscribe to a workflow run",
            "target_repo": "acme/web",
        }),
        root.workspace(),
        &[
            Answer::approval("doc_review"),
            // No note: an approval without one must still reach the plan.
            Answer::new("approve_scope", &[("scope_decision", "approved")]),
            // No feedback: the note on the plan is optional.
            Answer::new("approve_plan", &[("plan_decision", "approved")]),
            Answer::new(
                "implementation_review",
                &[
                    ("review_decision", "revise"),
                    ("revision_notes", "Handle the reviewer's requested change"),
                ],
            ),
            Answer::new(
                "implementation_review",
                &[("review_decision", "approve"), ("revision_notes", "none")],
            ),
            Answer::new(
                "learning_review",
                &[
                    ("learning_review_decision", "approve"),
                    ("revision_notes", "none"),
                ],
            ),
        ],
    )
    .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(execution.current_state.as_deref(), Some("finished"));
    // A blank optional note is an empty answer, not a missing variable.
    assert_eq!(
        execution.context.variables.get("scope_notes"),
        Some(&json!(""))
    );
    // The documents the agents returned are named in the later prompts, so no
    // step has to guess which file in `docs/` is meant.
    let prompt = |call: usize| {
        fs::read_to_string(harness_dir.join(format!("args.{call}"))).expect("read the prompt")
    };
    assert!(
        prompt(2).contains(requirements_doc),
        "doc_review names the requirements"
    );
    assert!(
        prompt(6).contains(requirements_doc),
        "plan names the requirements"
    );
    assert!(prompt(10).contains(plan_doc), "work names the plan");
}

#[tokio::test]
async fn the_open_mercato_pipeline_runs_through_its_review_loop_and_merge_gates() {
    let root = TempRoot::new();
    let harness = FakeHarness::installed_in(
        root.dir("harness"),
        &[
            "routed",            // discover
            "ready",             // intake
            "actionable",        // triage
            "claimed",           // claim
            "user_facing",       // assess_surface
            "shaped",            // shape_design
            "implemented",       // implement
            "pr_opened",         // open_pr
            "changes_requested", // code_review
            "updated",           // address_feedback
            "approved",          // code_review again
            "needs_qa",          // qa_routing
            "evidence_ready",    // qa_run
            "mergeable",         // premerge_check
            "merged",            // merge_pr
            "reconciled",        // housekeeping
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
