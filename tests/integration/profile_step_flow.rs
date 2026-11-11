//! A `profile` step runs its agent on a harness, checks the entries it
//! proposes and stores them in `step-profile.json` before the workflow goes
//! on (U6).
//!
//! The profiling agent is `tests/fixtures/harness/fake_harness.sh` behind the
//! real `claude-code` adapter; the `agent` steps are a recording executor
//! that notes what the stored profile held when each of them started. The
//! one test that drives the real `claude` is `#[ignore]`d.
//!
//! The last part reads the stored profile back (U7): every attempt of a
//! harness step starts on its entry, an escalation raises it, and a damaged
//! file or the conductor's old profile leaves the defaults.

use crate::engine_support;

use std::fs;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use engine_support::{envelope, event_type, workflow_file, FakeHarness, Harness};
use narsor_engine::domain::{
    ActionError, ActionResult, EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::actions::profile::{
    answer_schema, profile_candidates, read_profile_answer, PROFILE_REPROMPT,
};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, AgentExecutor, ClaudeCodeAdapter,
    Engine, HarnessRegistry, HarnessRun, OutputRejection, ProfileExecutor, METADATA_ENGINE_KEY,
    METADATA_OUTPUT_REJECTION_KEY,
};
use narsor_engine::persistence::{recover, ExecutionStore, StepProfileEntry};
use narsor_engine::workflow::load_from_file;
use narsor_protocol::chat::{ChatElement, TechnicalCategory};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A profile answer with `entries`, as the agent's closing JSON.
fn profile_answer(entries: Value) -> String {
    envelope(&format!(
        "I read the plan.\n{}",
        json!({ "entries": entries, "reason": "sized from the plan" })
    ))
}

/// What one `agent` step saw when it started.
#[derive(Debug, Clone)]
struct AgentStart {
    step: String,
    stored: Vec<StepProfileEntry>,
}

/// An `agent` executor that answers `exit` with a reply and records the
/// stored profile at the moment each step starts.
struct RecordingAgent {
    store: ExecutionStore,
    starts: Mutex<Vec<AgentStart>>,
}

impl ActionExecutor for RecordingAgent {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let stored = self
            .store
            .read_step_profile(
                request
                    .execution_id
                    .expect("the engine names the execution"),
            )
            .expect("read the step profile")
            .entries;
        let step = request
            .workflow
            .map(|workflow| workflow.step_id.to_string())
            .unwrap_or_default();
        self.starts
            .lock()
            .expect("starts")
            .push(AgentStart { step, stored });
        Box::pin(async {
            Ok(ActionOutcome::Completed(ActionResult {
                reply: Some("The plan: rework the scheduler and its locking.".to_string()),
                ..ActionResult::new("exit")
            }))
        })
    }
}

struct Fixture {
    harness: Harness,
    fake: FakeHarness,
    agent: Arc<RecordingAgent>,
}

impl Fixture {
    fn new() -> Self {
        let harness = Harness::new();
        let fake = FakeHarness::install(harness.root().join("bin"));
        let agent = Arc::new(RecordingAgent {
            store: harness.store(),
            starts: Mutex::new(Vec::new()),
        });
        Self {
            harness,
            fake,
            agent,
        }
    }

    fn engine(&self) -> Engine {
        let profile = ProfileExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(
            Arc::new(ClaudeCodeAdapter::with_program(self.fake.program())),
        ));
        Engine::new(self.harness.store())
            .with_executor(ActionKind::Agent, self.agent.clone())
            .with_executor(ActionKind::Profile, Arc::new(profile))
    }

    async fn run(&self, definition: &WorkflowDefinition) -> WorkflowExecution {
        self.engine()
            .start_and_run(definition, json!({}), self.harness.workspace())
            .await
            .expect("run the workflow")
    }

    fn stored(&self, execution_id: &str) -> Vec<StepProfileEntry> {
        self.harness
            .store()
            .read_step_profile(execution_id)
            .expect("read the step profile")
            .entries
    }

    fn events(&self, execution_id: &str) -> Vec<EventData> {
        self.harness
            .events(execution_id)
            .into_iter()
            .map(|event| event.data)
            .collect()
    }

    fn starts(&self) -> Vec<AgentStart> {
        self.agent.starts.lock().expect("starts").clone()
    }

    /// The Narsor entries of the conversation of `attempt`, as raw text and
    /// elements.
    fn narsor_entries(&self, execution_id: &str, attempt: &str) -> Vec<(String, ChatElement)> {
        self.harness
            .store()
            .conversation_store(execution_id)
            .read(attempt, 1, usize::MAX)
            .expect("read the conversation")
            .expect("the attempt has a conversation")
            .entries
            .into_iter()
            .flat_map(|entry| {
                let raw = String::from_utf8_lossy(&entry.raw).into_owned();
                entry
                    .elements
                    .into_iter()
                    .map(move |element| (raw.clone(), element))
            })
            .collect()
    }
}

/// The profile step's completed attempt and its reason.
fn profile_completion(events: &[EventData]) -> (String, String) {
    events
        .iter()
        .find_map(|event| match event {
            EventData::ActionCompleted {
                action_execution_id,
                reason: Some(reason),
                ..
            } if reason.starts_with("Step profile") => {
                Some((action_execution_id.clone(), reason.clone()))
            }
            _ => None,
        })
        .expect("the profile step completed")
}

fn the_fixture_workflow() -> WorkflowDefinition {
    workflow_file("v2/engine_profile_step.yaml")
}

/// A profile step with a retry policy, then one candidate.
fn retrying_workflow(fixture: &Fixture, extra: &str) -> WorkflowDefinition {
    let yaml = format!(
        r#"
schema: v2
name: profile-retry
version: 1
states:
  profile:
    action:
      type: profile
      harness: claude-code
      prompt: Judge the work of the ticket.
{extra}    retry_policy:
      max_attempts: 2
      backoff: 0s
    exit:
      next: work
  work:
    description: Does the work.
    action:
      type: agent
      model: medium
      effort: low
      ceiling: {{ model: large, effort: high }}
      prompt: Do it.
    exit:
      next: done
  done:
    description: Finished.
terminal_states:
  done: completed
"#
    );
    let path = fixture.harness.root().join("workflow.yaml");
    fs::write(&path, yaml).expect("write the workflow");
    load_from_file(&path).expect("load the workflow")
}

// ---------------------------------------------------------------------------
// The steps on the list (R2, R3)
// ---------------------------------------------------------------------------

/// AE1: `build` keeps its own model and effort and has no ceiling.
#[test]
fn a_pinned_step_is_not_on_the_list() {
    let ids: Vec<String> = profile_candidates(&the_fixture_workflow())
        .into_iter()
        .map(|candidate| candidate.step_id)
        .collect();
    assert_eq!(ids, ["plan", "review", "work"]);
}

/// AE2: the same `build` with a ceiling is on the list.
#[test]
fn a_step_with_a_ceiling_is_on_the_list_despite_its_own_values() {
    let fixture = Fixture::new();
    let yaml = fs::read_to_string(engine_support::fixtures().join("v2/engine_profile_step.yaml"))
        .expect("read the fixture")
        .replace(
            "      effort: low\n      prompt: Build it.",
            "      effort: low\n      ceiling: { model: large, effort: high }\n      prompt: Build it.",
        );
    let path = fixture.harness.root().join("ceiling.yaml");
    fs::write(&path, yaml).expect("write the workflow");
    let definition = load_from_file(&path).expect("load the workflow");

    let candidates = profile_candidates(&definition);
    let build = candidates
        .iter()
        .find(|candidate| candidate.step_id == "build")
        .expect("build is on the list");
    assert_eq!(build.default.model.as_deref(), Some("small"));
    assert_eq!(
        build.ceiling.as_ref().and_then(|c| c.effort.as_deref()),
        Some("high")
    );
}

// ---------------------------------------------------------------------------
// The flow
// ---------------------------------------------------------------------------

/// The prompt carries the author's prompt and the engine's data; the
/// checked profile is in the file before the next step starts; the
/// conversation shows the request, the clamp and the rationale (AE4).
#[tokio::test]
async fn a_profile_step_stores_its_profile_before_the_next_step_starts() {
    let fixture = Fixture::new();
    fixture.fake.answers_on_call(
        1,
        &profile_answer(json!([
            {"step": "work", "model": "large", "effort": "max",
             "rationale": "Touches the \u{1b}[31mscheduler\u{7}.", "confident": true},
            {"step": "review", "model": "small", "effort": "low",
             "rationale": "A short diff.", "confident": true},
            {"step": "build", "model": "large", "effort": "high",
             "rationale": "Pinned anyway.", "confident": true},
            {"step": "plan", "model": "large", "effort": "high",
             "rationale": "Not sure.", "confident": false},
        ])),
    );

    let execution = fixture.run(&the_fixture_workflow()).await;
    assert_eq!(execution.status, ExecutionStatus::Completed);

    // The prompt: the author's (rendered), the steps, the tiers, the efforts.
    let prompt = fixture.fake.args(1).join("\n");
    assert!(prompt.contains("Judge how hard each step is from the plan:"));
    assert!(prompt.contains("The plan: rework the scheduler and its locking."));
    assert!(
        prompt.contains(
            "- `work`; default: model `medium`, effort `low`; ceiling: model `large`, effort `high`"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("- `review`; default: model `medium`, effort `medium`; no ceiling"),
        "{prompt}"
    );
    assert!(!prompt.contains("- `build`"), "a pinned step is not listed");
    assert!(!prompt.contains("- `check`"), "a script is not listed");
    assert!(prompt.contains("Model tiers, cheapest first: `small`, `medium`, `large`, `max`"));
    assert!(prompt.contains("Efforts, cheapest first: `low`, `medium`, `high`, `max`"));
    assert!(prompt.contains("\"entries\""));

    // The file: two entries, the effort of `work` clamped to its ceiling.
    let stored = fixture.stored(&execution.id);
    assert_eq!(stored.len(), 2, "{stored:?}");
    let work = &stored[0];
    assert_eq!(work.step_id, "work");
    assert_eq!(work.model.as_deref(), Some("large"));
    assert_eq!(work.effort.as_deref(), Some("high"));
    assert_eq!(work.requested_effort.as_deref(), Some("max"));
    assert_eq!(work.profiled_by, "profile");
    assert!(!work.rationale.contains('\u{1b}'));
    assert_eq!(stored[1].step_id, "review");
    assert_eq!(stored[1].model.as_deref(), Some("small"));

    // Already there when the step after the profile step started.
    let starts = fixture.starts();
    let build = starts
        .iter()
        .find(|start| start.step == "build")
        .expect("build ran");
    assert_eq!(build.stored, stored);
    let plan = starts
        .iter()
        .find(|start| start.step == "plan")
        .expect("plan ran");
    assert!(plan.stored.is_empty(), "nothing before the profile step");

    // The step's reason, and the conversation entry.
    let events = fixture.events(&execution.id);
    let (attempt, reason) = profile_completion(&events);
    assert_eq!(reason, "Step profile: 2 stored, 2 skipped");
    let entries = fixture.narsor_entries(&execution.id, &attempt);
    let (text, element) = entries
        .iter()
        .find(|(_, element)| {
            matches!(element, ChatElement::Technical { summary, .. } if summary.starts_with("Step profile"))
        })
        .expect("the profile entry");
    assert!(matches!(
        element,
        ChatElement::Technical {
            category: TechnicalCategory::Process,
            ..
        }
    ));
    assert!(
        text.contains("effort `max` is above the ceiling: clamped to `high`"),
        "{text}"
    );
    assert!(
        text.contains("Rationale: Touches the [31mscheduler."),
        "{text}"
    );
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert!(text.contains("`build`: not stored"), "{text}");
    assert!(text.contains("the agent is not confident"), "{text}");
}

/// Every run replaces the file: an empty answer leaves an empty profile.
#[tokio::test]
async fn an_empty_profile_replaces_the_stored_one() {
    let fixture = Fixture::new();
    fixture.fake.answers_on_call(1, &profile_answer(json!([])));
    let definition = retrying_workflow(&fixture, "");
    let engine = fixture.engine();
    let execution = engine
        .start(&definition, json!({}), fixture.harness.workspace())
        .expect("start");
    fixture
        .harness
        .store()
        .write_step_profile(
            &execution.id,
            &[StepProfileEntry {
                step_id: "work".into(),
                model: Some("large".into()),
                effort: None,
                rationale: "an earlier run".into(),
                requested_model: Some("large".into()),
                requested_effort: None,
                profiled_by: "profile".into(),
                decided_at: chrono::Utc::now(),
            }],
        )
        .expect("write an earlier profile");

    let execution = engine
        .run(&definition, &execution.id)
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(fixture.stored(&execution.id).is_empty());
    let path = fixture
        .harness
        .store()
        .execution_dir(&execution.id)
        .join("step-profile.json");
    assert!(path.is_file(), "the empty profile is written, not removed");
    assert!(fixture.starts()[0].stored.is_empty());
}

/// R13: an answer without a profile is asked for again in the same
/// session; when the reprompt fails too, `retry_policy` runs the step again.
#[tokio::test]
async fn an_answer_without_a_profile_is_asked_again_and_then_retried() {
    let fixture = Fixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Work looks hard."))
        .answers_on_call(2, &envelope("Still no JSON, sorry."))
        .answers_on_call(
            3,
            &profile_answer(json!([
                {"step": "work", "model": "large", "effort": "high",
                 "rationale": "hard", "confident": true}
            ])),
        );

    let execution = fixture.run(&retrying_workflow(&fixture, "")).await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 3, "attempt, reprompt, retry");
    let reprompt = fixture.fake.args(2);
    assert!(
        reprompt
            .windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == "fake-session-1"),
        "{reprompt:?}"
    );
    assert!(reprompt.join("\n").contains(PROFILE_REPROMPT));
    assert!(!fixture.fake.args(3).iter().any(|arg| arg == "--resume"));

    let types: Vec<String> = fixture
        .harness
        .events(&execution.id)
        .iter()
        .map(event_type)
        .collect();
    for expected in [
        "action_output_rejected",
        "output_reprompted",
        "action_attempt_failed",
        "action_retrying",
    ] {
        assert!(types.contains(&expected.to_string()), "{types:?}");
    }
    assert!(!types.contains(&"problem_raised".to_string()), "{types:?}");
    assert_eq!(fixture.stored(&execution.id).len(), 1);
}

/// With `retry_policy` used up, an unusable answer fails the step; no
/// profile is written.
#[tokio::test]
async fn an_unusable_answer_fails_the_step_once_the_retries_are_used_up() {
    let fixture = Fixture::new();
    for call in 1..=4 {
        fixture
            .fake
            .answers_on_call(call, &envelope("No JSON here."));
    }

    let execution = fixture.run(&retrying_workflow(&fixture, "")).await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(
        fixture.fake.calls(),
        4,
        "two attempts, each asked again once"
    );
    let types: Vec<String> = fixture
        .harness
        .events(&execution.id)
        .iter()
        .map(event_type)
        .collect();
    assert!(types.contains(&"action_failed".to_string()), "{types:?}");
    assert!(fixture.stored(&execution.id).is_empty());
    assert!(fixture.starts().is_empty(), "the next step never starts");
}

/// What a daemon stopped between a rejected answer of the profile step's
/// attempt `attempt` and its reprompt leaves behind: the attempts in the
/// log, the last one closed by its rejection, and the engine's record of it.
/// Startup recovery has run; the next pass continues with the reprompt.
async fn stopped_before_the_reprompt(
    fixture: &Fixture,
    engine: &Engine,
    definition: &WorkflowDefinition,
    attempt: u32,
) -> WorkflowExecution {
    let store = fixture.harness.store();
    let mut execution = engine
        .start(definition, json!({}), fixture.harness.workspace())
        .expect("start");
    let rejected = format!("attempt-{attempt}");
    let mut events = vec![
        EventData::StateEntered {
            state_id: "profile".to_string(),
        },
        EventData::ActionStarted {
            action_execution_id: "attempt-1".to_string(),
            state_id: "profile".to_string(),
            action_type: "profile".to_string(),
            decision_by: None,
        },
    ];
    for earlier in 1..attempt {
        events.push(EventData::ActionAttemptFailed {
            action_execution_id: format!("attempt-{earlier}"),
            state_id: "profile".to_string(),
            error: "the harness crashed".to_string(),
            retry_in_ms: 0,
        });
        events.push(EventData::ActionRetrying {
            action_execution_id: format!("attempt-{}", earlier + 1),
            retry_count: earlier,
            state_id: Some("profile".to_string()),
        });
    }
    events.push(EventData::ActionOutputRejected {
        action_execution_id: rejected.clone(),
        state_id: "profile".to_string(),
        reason: "no step profile in the answer".to_string(),
        answer_tail: None,
    });
    let log = store.event_log(&execution.id);
    for data in events {
        log.append_next(&execution.id, data).expect("append");
    }
    let rejection = OutputRejection {
        state_id: "profile".to_string(),
        action_execution_id: rejected,
        reason: "no step profile in the answer".to_string(),
        answer_tail: None,
        reply: None,
        harness: Some("claude-code".to_string()),
        harness_session_id: Some("session-7".to_string()),
        attempt: Some(attempt),
    };
    execution.current_state = Some("profile".to_string());
    execution.metadata = json!({
        METADATA_ENGINE_KEY: { METADATA_OUTPUT_REJECTION_KEY: rejection },
    });
    store
        .save_execution(&execution)
        .expect("save the execution");
    recover(&store).expect("recover");
    execution
}

/// R13 across a restart: a reprompt continued after the daemon stopped
/// that is still unusable is a failed attempt under `retry_policy`, as
/// without the restart — the next attempt runs, not the conductor's rescue.
#[tokio::test]
async fn a_reprompt_still_unusable_after_a_restart_is_retried() {
    let fixture = Fixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Still no JSON, sorry."))
        .answers_on_call(
            2,
            &profile_answer(json!([
                {"step": "work", "model": "large", "effort": "high",
                 "rationale": "hard", "confident": true}
            ])),
        );
    let engine = fixture.engine();
    let definition = retrying_workflow(&fixture, "");
    let execution = stopped_before_the_reprompt(&fixture, &engine, &definition, 1).await;

    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("resume");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 2, "the reprompt, then the retry");
    assert!(
        fixture
            .fake
            .args(1)
            .windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == "session-7"),
        "{:?}",
        fixture.fake.args(1)
    );
    assert!(!fixture.fake.args(2).iter().any(|arg| arg == "--resume"));
    let events = fixture.events(&execution.id);
    let reprompt = events
        .iter()
        .find_map(|event| match event {
            EventData::OutputReprompted {
                action_execution_id,
                ..
            } => Some(action_execution_id.clone()),
            _ => None,
        })
        .expect("the reprompt ran");
    let failed = events
        .iter()
        .find_map(|event| match event {
            EventData::ActionAttemptFailed {
                action_execution_id,
                ..
            } => Some(action_execution_id.clone()),
            _ => None,
        })
        .expect("an attempt failed");
    assert_eq!(failed, reprompt, "the reprompt is the attempt that failed");
    let retry_count = events
        .iter()
        .find_map(|event| match event {
            EventData::ActionRetrying { retry_count, .. } => Some(*retry_count),
            _ => None,
        })
        .expect("the step was retried");
    assert_eq!(retry_count, 1, "the second attempt of the policy");
    let types: Vec<String> = fixture
        .harness
        .events(&execution.id)
        .iter()
        .map(event_type)
        .collect();
    assert!(
        types.contains(&"output_reprompted".to_string()),
        "{types:?}"
    );
    assert!(!types.contains(&"problem_raised".to_string()), "{types:?}");
    assert_eq!(fixture.stored(&execution.id).len(), 1);
    assert_eq!(fixture.starts().len(), 1, "the next step ran");
}

/// With `retry_policy` used up, a reprompt continued after a restart that is
/// still unusable fails the step, as without the restart — no rescue, no
/// problem for the operator.
#[tokio::test]
async fn a_reprompt_still_unusable_after_a_restart_fails_once_the_retries_are_used_up() {
    let fixture = Fixture::new();
    fixture
        .fake
        .answers_on_call(1, &envelope("Still no JSON, sorry."));
    let engine = fixture.engine();
    let definition = retrying_workflow(&fixture, "");
    let execution = stopped_before_the_reprompt(&fixture, &engine, &definition, 2).await;

    let execution = engine
        .resume(&definition, &execution.id)
        .await
        .expect("resume");

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(fixture.fake.calls(), 1, "only the reprompt ran");
    let types: Vec<String> = fixture
        .harness
        .events(&execution.id)
        .iter()
        .map(event_type)
        .collect();
    assert!(types.contains(&"action_failed".to_string()), "{types:?}");
    assert!(!types.contains(&"problem_raised".to_string()), "{types:?}");
    assert_eq!(
        types
            .iter()
            .filter(|kind| *kind == "action_retrying")
            .count(),
        1,
        "only the retry before the restart: {types:?}"
    );
    assert!(fixture.stored(&execution.id).is_empty());
    assert!(fixture.starts().is_empty(), "the next step never starts");
}

/// AE7: a profiling agent that runs out of time, with `retry_policy` used
/// up, fails the step like an agent step.
#[tokio::test]
async fn a_timed_out_profile_step_fails_like_an_agent_step() {
    let fixture = Fixture::new();
    fixture.fake.sleeps(30);

    let execution = fixture
        .run(&retrying_workflow(&fixture, "    timeout: 1\n"))
        .await;

    assert_eq!(execution.status, ExecutionStatus::Failed);
    assert_eq!(fixture.fake.calls(), 2, "both attempts of the retry policy");
    let failure = fixture
        .events(&execution.id)
        .into_iter()
        .find_map(|event| match event {
            EventData::ActionFailed { error, .. } => Some(error),
            _ => None,
        })
        .expect("the action failed");
    assert!(failure.contains("time limit"), "{failure}");
    assert!(fixture.starts().is_empty());
}

// ---------------------------------------------------------------------------
// The real harness — deliberately ignored
// ---------------------------------------------------------------------------

/// Hands the profile answer schema to the real `claude` as `--json-schema`:
/// the API must take it (a flat object, no top-level `anyOf`), and the
/// structured output it returns must read as a profile.
#[test]
#[ignore = "drives the real `claude` binary: costs money, needs credentials"]
fn the_real_claude_binary_takes_the_profile_answer_schema() {
    let Some(claude) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("claude"))
            .find(|candidate| candidate.is_file())
    }) else {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    };

    let output = std::process::Command::new(claude)
        .args([
            "-p",
            "--output-format",
            "json",
            "--model",
            "haiku",
            "--json-schema",
            &answer_schema().to_string(),
            "Do not use any tools. Propose a step profile with one entry: step `work`, \
             model `large`, effort `high`, rationale `test`, confident true.",
        ])
        .output()
        .expect("run claude");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: Value = serde_json::from_str(stdout.trim()).expect("one JSON result");
    assert_eq!(result["is_error"], json!(false), "{result}");

    let run = HarnessRun {
        structured_output: result.get("structured_output").cloned(),
        ..HarnessRun::default()
    };
    let answer = read_profile_answer(&run).expect("the structured output is a profile");
    assert_eq!(answer.entries.len(), 1, "{result}");
}

// ---------------------------------------------------------------------------
// The stored profile at every attempt (U7)
// ---------------------------------------------------------------------------

use std::collections::{HashMap, VecDeque};

use engine_support::{exit, AgentCall, AgentTurn, ScriptedAgentExecutor};

/// A `profile` executor that answers each run with the next of `runs`, as
/// the real one does: the label `exit`, the checked entries in its data.
struct ScriptedProfile {
    runs: Mutex<VecDeque<Vec<StepProfileEntry>>>,
}

impl ActionExecutor for ScriptedProfile {
    fn execute<'a>(
        &'a self,
        _request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let entries = self
            .runs
            .lock()
            .expect("runs")
            .pop_front()
            .expect("the profile step ran more often than scripted");
        Box::pin(async move {
            Ok(ActionOutcome::Completed(ActionResult {
                reason: "Step profile".to_string(),
                data: json!({ "step_profile": entries }),
                ..ActionResult::new("exit")
            }))
        })
    }
}

/// The scripted agent, which writes a new step profile during its `nth`
/// call (1-based), as if a `profile` step had run meanwhile: the attempts
/// after that call see it.
struct WritingAgent {
    inner: ScriptedAgentExecutor,
    store: ExecutionStore,
    writes: Mutex<HashMap<usize, Vec<StepProfileEntry>>>,
    calls: Mutex<usize>,
}

impl ActionExecutor for WritingAgent {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        let nth = {
            let mut calls = self.calls.lock().expect("calls");
            *calls += 1;
            *calls
        };
        if let Some(entries) = self.writes.lock().expect("writes").remove(&nth) {
            self.store
                .write_step_profile(
                    request
                        .execution_id
                        .expect("the engine names the execution"),
                    &entries,
                )
                .expect("write the step profile");
        }
        self.inner.execute(request)
    }
}

/// An entry for `step`, as a `profile` step stores it.
fn stored(step: &str, model: Option<&str>, effort: Option<&str>) -> StepProfileEntry {
    StepProfileEntry {
        step_id: step.into(),
        model: model.map(Into::into),
        effort: effort.map(Into::into),
        rationale: "test".into(),
        requested_model: model.map(Into::into),
        requested_effort: effort.map(Into::into),
        profiled_by: "profile".into(),
        decided_at: chrono::Utc::now(),
    }
}

/// An engine whose `profile` step answers `runs` and whose agent plays
/// `turns`, writing `writes` during the given calls.
struct Steps {
    harness: Harness,
    agent: Arc<WritingAgent>,
    engine: Engine,
}

impl Steps {
    fn new(
        runs: Vec<Vec<StepProfileEntry>>,
        turns: Vec<AgentTurn>,
        writes: Vec<(usize, Vec<StepProfileEntry>)>,
    ) -> Self {
        let harness = Harness::new();
        let agent = Arc::new(WritingAgent {
            inner: ScriptedAgentExecutor::new(turns),
            store: harness.store(),
            writes: Mutex::new(writes.into_iter().collect()),
            calls: Mutex::new(0),
        });
        let engine = harness
            .engine()
            .with_escalation_threshold(3)
            .with_executor(ActionKind::Agent, agent.clone())
            .with_executor(
                ActionKind::Profile,
                Arc::new(ScriptedProfile {
                    runs: Mutex::new(runs.into()),
                }),
            );
        Self {
            harness,
            agent,
            engine,
        }
    }

    /// Runs [`agent_steps`] with `entries` stored before the first step.
    async fn run_agent_steps(&self, entries: &[StepProfileEntry]) -> WorkflowExecution {
        let definition = agent_steps(&self.harness);
        let execution = self
            .engine
            .start(&definition, json!({}), self.harness.workspace())
            .expect("start");
        self.harness
            .store()
            .write_step_profile(&execution.id, entries)
            .expect("write the step profile");
        self.engine
            .run(&definition, &execution.id)
            .await
            .expect("run")
    }

    async fn run(&self, file: &str) -> WorkflowExecution {
        self.engine
            .start_and_run(&workflow_file(file), json!({}), self.harness.workspace())
            .await
            .expect("run the workflow")
    }

    /// The model and effort of every call whose prompt starts with `prompt`.
    fn settings_of(&self, prompt: &str) -> Vec<(Option<String>, Option<String>)> {
        self.agent
            .inner
            .calls()
            .into_iter()
            .filter(|call: &AgentCall| call.prompt.starts_with(prompt))
            .map(|call| (call.model, call.effort))
            .collect()
    }

    /// The `step_profile` events: (message, context).
    fn profile_events(&self, execution_id: &str) -> Vec<(String, Value)> {
        self.harness
            .events(execution_id)
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::Error { message, context } if context.get("step_profile").is_some() => {
                    Some((message, context))
                }
                _ => None,
            })
            .collect()
    }
}

fn setting(model: &str, effort: &str) -> (Option<String>, Option<String>) {
    (Some(model.to_string()), Some(effort.to_string()))
}

/// AE5 and the entry for `work`: a step with an entry runs on it; one
/// without runs on its own values, else on the globals'.
#[tokio::test]
async fn a_step_starts_on_its_stored_entry_or_on_its_defaults() {
    let steps = Steps::new(
        vec![vec![stored("work", Some("large"), Some("high"))]],
        vec![exit(); 4],
        vec![],
    );

    let execution = steps.run("v2/engine_profile_step.yaml").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(steps.settings_of("Do it"), [setting("large", "high")]);
    assert_eq!(steps.settings_of("Build it"), [setting("small", "low")]);
    // `review` names nothing: the globals' defaults.
    assert_eq!(
        steps.settings_of("Review it"),
        [setting("medium", "medium")]
    );
    assert!(steps.profile_events(&execution.id).is_empty());
}

/// F3: a `profile` step that runs again replaces the stored profile, and an
/// entry written after the step was first entered applies at its next
/// entry.
#[tokio::test]
async fn a_profile_step_run_again_in_a_loop_replaces_the_profile() {
    let steps = Steps::new(
        vec![
            vec![],
            vec![stored("work", Some("large"), Some("high"))],
            vec![stored("work", Some("small"), None)],
        ],
        vec![
            AgentTurn::label("again", "profile again"),
            AgentTurn::label("again", "profile again"),
            AgentTurn::label("ready", "done"),
            exit(),
        ],
        vec![],
    );

    let execution = steps.run("v2/engine_profile_loop.yaml").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        steps.settings_of("Do it"),
        [
            setting("medium", "low"),
            setting("large", "high"),
            setting("small", "low"),
        ]
    );
    let file = steps
        .harness
        .store()
        .read_step_profile(&execution.id)
        .expect("read the step profile")
        .entries;
    assert_eq!(file.len(), 1, "{file:?}");
    assert_eq!(file[0].model.as_deref(), Some("small"));
}

/// AE6, as the operator decided it: in one entry `work` fails three times
/// and runs on an effort one notch above the stored one.
#[tokio::test]
async fn an_escalation_raises_the_stored_effort() {
    let steps = Steps::new(
        vec![vec![stored("work", None, Some("medium"))]],
        vec![
            AgentTurn::Fail,
            AgentTurn::Fail,
            AgentTurn::Fail,
            AgentTurn::label("ready", "done"),
            exit(),
        ],
        vec![],
    );

    let execution = steps.run("v2/engine_profile_loop.yaml").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        steps.settings_of("Do it"),
        [
            setting("medium", "medium"),
            setting("medium", "medium"),
            setting("medium", "medium"),
            setting("medium", "high"),
        ]
    );
    let events = steps.profile_events(&execution.id);
    assert!(
        events
            .iter()
            .any(|(message, context)| message.contains("is raised")
                && context["step_profile"]["step"] == "work"),
        "{events:?}"
    );
}

/// R12: a profile written while a step is failing does not reset its count
/// of failed attempts; the next attempt raises the new value.
#[tokio::test]
async fn a_new_profile_keeps_the_count_of_failed_attempts() {
    let steps = Steps::new(
        vec![vec![stored("work", Some("large"), Some("high"))]],
        vec![
            AgentTurn::Fail,
            AgentTurn::Fail,
            AgentTurn::Fail,
            AgentTurn::label("ready", "done"),
            exit(),
        ],
        // During the second attempt of `work`.
        vec![(2, vec![stored("work", None, Some("low"))])],
    );

    let execution = steps.run("v2/engine_profile_loop.yaml").await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(
        steps.settings_of("Do it"),
        [
            setting("large", "high"),
            setting("large", "high"),
            setting("medium", "low"),
            // Three failures in a row, counted across the new profile.
            setting("medium", "medium"),
        ]
    );
}

/// A damaged step profile is no profile: every step runs on its defaults,
/// and the execution records it once.
#[tokio::test]
async fn a_damaged_step_profile_means_defaults_and_one_warning() {
    let steps = Steps::new(vec![], vec![exit(); 2], vec![]);
    let definition = agent_steps(&steps.harness);
    let execution = steps
        .engine
        .start(&definition, json!({}), steps.harness.workspace())
        .expect("start");
    let path = steps
        .harness
        .store()
        .execution_dir(&execution.id)
        .join("step-profile.json");
    fs::write(&path, "{ not json").expect("damage the step profile");

    let execution = steps
        .engine
        .run(&definition, &execution.id)
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(steps.settings_of("Do it"), [setting("medium", "low")]);
    assert_eq!(
        steps.settings_of("Review it"),
        [setting("medium", "medium")]
    );
    // Both steps read the file; one warning.
    let events = steps.profile_events(&execution.id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].0.contains("could not be read"), "{}", events[0].0);
}

/// The profile the conductor stored before this version is not read.
#[tokio::test]
async fn an_old_conductor_profile_is_ignored() {
    let steps = Steps::new(vec![], vec![exit(); 2], vec![]);
    let definition = agent_steps(&steps.harness);
    let execution = steps
        .engine
        .start(&definition, json!({}), steps.harness.workspace())
        .expect("start");
    let conductor = steps
        .harness
        .store()
        .execution_dir(&execution.id)
        .join("conductor");
    fs::create_dir_all(&conductor).expect("the conductor directory");
    fs::write(
        conductor.join("profile.json"),
        json!({
            "version": 1,
            "entries": [{
                "step_id": "work",
                "setting": { "model": "large", "effort": "high" },
                "rationale": "old",
                "decided_at": "2026-01-01T00:00:00Z",
            }],
        })
        .to_string(),
    )
    .expect("write the old profile");

    let execution = steps
        .engine
        .run(&definition, &execution.id)
        .await
        .expect("run");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(steps.settings_of("Do it"), [setting("medium", "low")]);
    assert!(steps.profile_events(&execution.id).is_empty());
}

/// The fake profiling agent's answer reaches the argv of the next agent
/// step's harness.
#[tokio::test]
async fn the_next_agent_step_starts_its_harness_on_the_stored_profile() {
    let fixture = Fixture::new();
    fixture
        .fake
        .answers_on_call(
            1,
            &profile_answer(json!([
                {"step": "work", "model": "large", "effort": "high",
                 "rationale": "hard", "confident": true}
            ])),
        )
        .answers_on_call(2, &envelope(r#"{"reason":"done"}"#));
    let adapter = || {
        HarnessRegistry::defaults().with_adapter(Arc::new(ClaudeCodeAdapter::with_program(
            fixture.fake.program(),
        )))
    };
    let engine = Engine::new(fixture.harness.store())
        .with_executor(
            ActionKind::Agent,
            Arc::new(AgentExecutor::with_harnesses(adapter())),
        )
        .with_executor(
            ActionKind::Profile,
            Arc::new(ProfileExecutor::with_harnesses(adapter())),
        );
    let definition = retrying_workflow(&fixture, "");

    let execution = engine
        .start_and_run(&definition, json!({}), fixture.harness.workspace())
        .await
        .expect("run the workflow");

    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert_eq!(fixture.fake.calls(), 2);
    let args = fixture.fake.args(2);
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "opus"),
        "{args:?}"
    );
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--effort" && pair[1] == "high"),
        "{args:?}"
    );
}

/// A stored entry the tiers or the scale do not know keeps the default and
/// is recorded, naming the step that profiled it.
#[tokio::test]
async fn a_stored_entry_that_cannot_be_used_keeps_the_default_and_is_recorded() {
    let steps = Steps::new(vec![], vec![exit(); 2], vec![]);

    let execution = steps
        .run_agent_steps(&[stored("work", Some("mystery"), Some("max"))])
        .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    // "max" is above the ceiling's "high": clamped; the model is rejected.
    assert_eq!(steps.settings_of("Do it"), [setting("medium", "high")]);
    let events = steps.profile_events(&execution.id);
    assert!(
        events.iter().any(|(message, _)| message.contains(
            "the profile from step 'profile' named a model or effort that cannot be used"
        )),
        "{events:?}"
    );
}

/// A failing step raised up to its ceiling announces the limit; a step
/// without a ceiling never does.
#[tokio::test]
async fn the_limit_is_announced_only_for_a_step_with_a_ceiling() {
    let fails = || vec![AgentTurn::Fail, AgentTurn::Fail, AgentTurn::Fail, exit()];
    let steps = Steps::new(vec![], [fails(), fails()].concat(), vec![]);

    let execution = steps
        .run_agent_steps(&[
            stored("work", Some("large"), Some("high")),
            stored("review", Some("medium"), Some("low")),
        ])
        .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let events = steps.profile_events(&execution.id);
    let limits: Vec<_> = events
        .iter()
        .filter(|(message, _)| message.contains("already runs on its ceiling"))
        .collect();
    assert_eq!(limits.len(), 1, "{events:?}");
    assert_eq!(limits[0].1["step_profile"]["step"], "work");
}

/// A reprompt resumes the session on the stored model and effort, not on
/// the workflow's default.
#[tokio::test]
async fn a_reprompt_resumes_the_session_on_the_stored_profile() {
    let steps = Steps::new(
        vec![],
        vec![
            AgentTurn::NoLabel {
                reply: Some("no label"),
                session: true,
            },
            exit(),
            exit(),
        ],
        vec![],
    );

    let execution = steps
        .run_agent_steps(&[stored("work", Some("large"), Some("high"))])
        .await;

    assert_eq!(execution.status, ExecutionStatus::Completed);
    let work: Vec<AgentCall> = steps
        .agent
        .inner
        .calls()
        .into_iter()
        // `review` runs on the globals' `medium`.
        .filter(|call| call.model.as_deref() == Some("large"))
        .collect();
    assert_eq!(work.len(), 2, "{work:?}");
    assert!(work[0].resume_session.is_none());
    assert!(work[1].resume_session.is_some());
    assert_eq!(
        (work[1].model.clone(), work[1].effort.clone()),
        setting("large", "high")
    );
}

/// Two agent steps and no `profile` step: `work` (with a ceiling) and
/// `review` (on the globals' defaults).
fn agent_steps(harness: &Harness) -> WorkflowDefinition {
    let yaml = r#"
schema: v2
name: agent-steps
version: 1
globals:
  default_model: medium
  default_effort: medium
states:
  work:
    action:
      type: agent
      model: medium
      effort: low
      ceiling: { model: large, effort: high }
      prompt: Do it.
    retry_policy:
      max_attempts: 5
      backoff: 0s
    exit:
      next: review
  review:
    action:
      type: agent
      prompt: Review it.
    retry_policy:
      max_attempts: 5
      backoff: 0s
    exit:
      next: done
  done:
    description: Finished.
terminal_states:
  done: completed
"#;
    let path = harness.root().join("agents.yaml");
    fs::write(&path, yaml).expect("write the workflow");
    load_from_file(&path).expect("load the workflow")
}
