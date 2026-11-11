//! A `profile` step runs its agent on a harness, checks the entries it
//! proposes and stores them in `step-profile.json` before the workflow goes
//! on (U6).
//!
//! The profiling agent is `tests/fixtures/harness/fake_harness.sh` behind the
//! real `claude-code` adapter; the `agent` steps are a recording executor
//! that notes what the stored profile held when each of them started. The
//! one test that drives the real `claude` is `#[ignore]`d.

use crate::engine_support;

use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use engine_support::{event_type, workflow_file, Harness};
use narsor_engine::domain::{
    ActionError, ActionResult, EventData, ExecutionStatus, WorkflowDefinition, WorkflowExecution,
};
use narsor_engine::engine::actions::profile::{
    answer_schema, profile_candidates, read_profile_answer, PROFILE_REPROMPT,
};
use narsor_engine::engine::{
    ActionExecutor, ActionKind, ActionOutcome, ActionRequest, ClaudeCodeAdapter, Engine,
    HarnessRegistry, HarnessRun, ProfileExecutor,
};
use narsor_engine::persistence::{ExecutionStore, StepProfileEntry};
use narsor_engine::workflow::load_from_file;
use narsor_protocol::chat::{ChatElement, TechnicalCategory};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The fake harness CLI, installed in a directory of its own.
struct FakeHarness {
    dir: PathBuf,
    program: PathBuf,
}

impl FakeHarness {
    /// Copies the fixture script into `dir`, through a temporary name and a
    /// rename (see `actions_agent.rs` for why).
    fn install(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).expect("create the harness directory");
        let source = engine_support::fixtures().join("harness/fake_harness.sh");
        let program = dir.join("fake_harness.sh");
        let temp = dir.join(format!(".fake_harness.sh.{}.tmp", uuid::Uuid::new_v4()));
        fs::copy(&source, &temp).expect("copy the fake harness");
        fs::rename(&temp, &program).expect("move the fake harness into place");
        Self { dir, program }
    }

    /// What the harness prints on the `nth` call (1-based).
    fn answers_on_call(&self, nth: usize, stdout: &str) -> &Self {
        fs::write(self.dir.join(format!("stdout.{nth}")), stdout).expect("write the answer");
        self
    }

    /// Makes the harness sleep after answering, on every call.
    fn sleeps(&self, seconds: u64) -> &Self {
        fs::write(self.dir.join("sleep"), seconds.to_string()).expect("write the sleep");
        self
    }

    /// The arguments of the `nth` call.
    fn args(&self, nth: usize) -> Vec<String> {
        fs::read_to_string(self.dir.join(format!("args.{nth}")))
            .expect("the harness recorded its arguments")
            .split("\n--%ARG%--\n")
            .filter(|arg| !arg.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// How many times the harness was called.
    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("count"))
            .map(|raw| raw.trim().parse().expect("the count is a number"))
            .unwrap_or(0)
    }
}

/// A single `stream-json` `result` line carrying `text`.
fn envelope(text: &str) -> String {
    json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "fake-session-1",
        "result": text,
    })
    .to_string()
}

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
        let profile =
            ProfileExecutor::with_harnesses(HarnessRegistry::defaults().with_adapter(Arc::new(
                ClaudeCodeAdapter::with_program(self.fake.program.to_string_lossy().into_owned()),
            )));
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
