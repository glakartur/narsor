//! The example catalogue (`examples/catalog/`): it loads with no problem,
//! and its scripts work against a fake source. Each script reads the
//! answers of its source from `FAKE_SOURCE_DIR`, records the changes it would
//! send in `writes.log` there, and calls a stand-in `narsor-cli` that records
//! its arguments. The input follows version 3 of the script contract. The
//! Jira `check` script (`always`) must print a document that the contract
//! accepts; a PR/MR `check` script (`on_episodes`) checks one queue entry and
//! answers its gate with `narsor-cli task decide`; its `in` and `out` scripts
//! add and remove the review label, only when needed (R13).
//!
//! The script tests run the real scripts with `sh` and `jq`, so they are
//! `#[ignore]`d: `cargo test -- --include-ignored` runs them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use narsor_engine::automations::{AutomationCatalog, HookPhase};
use narsor_engine::domain::TaskId;
use narsor_engine::observers::{
    parse_script_output, ScriptChoice, ScriptEntry, ScriptOutput, ScriptVariable,
    SCRIPT_CONTRACT_VERSION,
};
use narsor_engine::workflow::load_from_str;
use narsor_engine::workflows::builtin;
use serde_json::{json, Value};

/// The example catalogue of the repository.
fn example_catalog() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/catalog")
}

/// A new installation copies `examples/catalog/` into its catalogue: every
/// file loads, both queues are active, and the review automations have all
/// three scripts. The Jira queue creates tasks of the built-in `template`.
#[test]
fn the_example_catalog_loads_with_no_problems() {
    let load = AutomationCatalog::new(example_catalog()).load(|id| {
        let workflow = builtin(id).ok_or_else(|| format!("'{id}' is not built in"))?;
        load_from_str(workflow.source).map_err(|error| error.to_string())
    });

    assert_eq!(load.problems, Vec::new());
    assert_eq!(
        load.queues["mr-review"].automations,
        ["gitlab-mr-label", "github-pr-label"]
    );
    for review in ["gitlab-mr-label", "github-pr-label"] {
        assert!(
            HookPhase::ALL
                .iter()
                .all(|phase| load.automations[review].has(*phase)),
            "{review}"
        );
    }
    let intake = load.queues["jira-intake"].tasks.as_ref().expect("tasks");
    assert_eq!(intake.effect.workflow_id, "template");
    assert!(!load.automations["jira-new-issues"].has(HookPhase::In));
}

/// The example `runtime.yaml` defines a task image for the Docker work mode
/// whose scripts are files of the catalogue: it has no problem and seals
/// with every script's text, in the order the image installs them.
#[test]
fn the_example_task_image_seals_with_its_scripts() {
    let runtime = narsor_engine::environment::EnvironmentCatalog::new(example_catalog())
        .runtime()
        .expect("runtime.yaml parses")
        .expect("runtime.yaml exists");
    assert_eq!(runtime.problems(), Vec::<String>::new());

    let sealed = runtime
        .task_image
        .expect("a task image")
        .seal(&example_catalog())
        .expect("every script loads");

    let names: Vec<&str> = sealed.components.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["base-tools", "claude-code", "glab"]);
    assert!(sealed.components[1]
        .install
        .contains("/usr/local/bin/claude"));
    assert!(sealed.components[1].version.is_some());
}

/// A temporary directory with a fake source and a stand-in `narsor-cli`.
struct Sandbox {
    path: PathBuf,
}

/// What one run of an example script gave.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// The arguments of every `narsor-cli` call, one list per call.
    calls: Vec<Vec<String>>,
}

impl Run {
    fn output(&self) -> ScriptOutput {
        assert_eq!(self.code, Some(0), "stderr: {}", self.stderr);
        parse_script_output(self.stdout.as_bytes())
            .unwrap_or_else(|error| panic!("{error}: {:?}", self.stdout))
    }
}

impl Sandbox {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-observer-examples-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("source")).expect("create the fake source");
        fs::create_dir_all(path.join("bin")).expect("create the stand-in bin");
        let stub = path.join("bin/narsor-cli");
        // One argument per line, a blank line after each call. Call number
        // `fail_from` and every later one fail (see `Sandbox::fail_from`).
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> '{log}'\n\
                 printf '\\n' >> '{log}'\n\
                 count=$(( $(cat '{count}' 2>/dev/null || echo 0) + 1 )); echo $count > '{count}'\n\
                 if [ -f '{fail}' ] && [ $count -ge $(cat '{fail}') ]; then echo 'refused' >&2; exit 1; fi\n\
                 echo 'delivered'\n",
                log = path.join("calls").display(),
                count = path.join("count").display(),
                fail = path.join("fail_from").display(),
            ),
        )
        .expect("write the stand-in narsor-cli");
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod narsor-cli");
        Self { path }
    }

    fn answer(&self, name: &str, body: &Value) {
        fs::write(
            self.path.join("source").join(format!("{name}.json")),
            body.to_string(),
        )
        .expect("write a fake answer");
    }

    /// The stand-in `narsor-cli` fails from its call number `call` (from 1)
    /// on.
    fn fail_from(&self, call: usize) {
        fs::write(self.path.join("fail_from"), call.to_string()).expect("write fail_from");
    }

    /// The source answers `name` with HTTP `code`.
    fn status(&self, name: &str, code: u16) {
        fs::write(
            self.path.join("source").join(format!("{name}.status")),
            code.to_string(),
        )
        .expect("write a fake status");
    }

    /// The changes the scripts sent to the fake source, one line each.
    fn writes(&self) -> Vec<String> {
        fs::read_to_string(self.path.join("source/writes.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Runs the `phase` script of `automation`; `input` must name the same
    /// phase.
    fn run(&self, automation: &str, phase: HookPhase, input: &Value) -> Run {
        let script = example_catalog()
            .join("automations")
            .join(automation)
            .join(phase.script_file());
        let path = format!(
            "{}:{}",
            self.path.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("sh")
            .arg(&script)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.path)
            .env("FAKE_SOURCE_DIR", self.path.join("source"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.to_string().as_bytes())
            .expect("write the input");
        let output = child.wait_with_output().expect("wait for the script");
        let log = fs::read_to_string(self.path.join("calls")).unwrap_or_default();
        let calls = log
            .split("\n\n")
            .filter(|call| !call.is_empty())
            .map(|call| call.lines().map(str::to_string).collect())
            .collect();
        Run {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            calls,
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The `check` input; `entry` is `Value::Null` for an `always` run, which
/// has no `entry` field.
fn input(condition: Value, state: Value, entry: Value) -> Value {
    let mut input = json!({
        "contract_version": SCRIPT_CONTRACT_VERSION,
        "phase": "check",
        "queue": "example-queue",
        "automation": "example",
        "condition": condition,
        "state": state,
        "first_run": state.is_null(),
        "import_backlog": false,
    });
    if !entry.is_null() {
        input["entry"] = entry;
    }
    input
}

fn jira_issue(id: u64, key: &str) -> Value {
    json!({
        "id": id.to_string(),
        "key": key,
        "fields": {
            "summary": format!("Summary of {key}"),
            "issuetype": {"name": "Bug"},
            "priority": {"name": "High"},
            "status": {"name": "Open"},
            "labels": [],
            "description": "text",
            "reporter": {"displayName": "Jan Kowalski", "emailAddress": "jan@example.com"}
        }
    })
}

fn jira_condition() -> Value {
    json!({"base_url": "https://jira.example.com", "jql": "project = ABC", "fields": ["key", "summary"]})
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn jira_first_run_records_the_newest_issue_and_creates_no_task() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "search",
        &json!({"issues": [jira_issue(105, "ABC-105"), jira_issue(101, "ABC-101")]}),
    );

    let run = sandbox.run(
        JIRA,
        HookPhase::Check,
        &input(jira_condition(), Value::Null, Value::Null),
    );

    let output = run.output();
    assert_eq!(output.state, json!({"last_id": 105}));
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn jira_creates_one_task_per_new_issue_with_only_the_mapped_fields() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "search",
        &json!({"issues": [
            jira_issue(102, "ABC-102"),
            jira_issue(100, "ABC-100"),
            jira_issue(101, "ABC-101"),
        ]}),
    );

    let run = sandbox.run(
        JIRA,
        HookPhase::Check,
        &input(jira_condition(), json!({"last_id": 100}), Value::Null),
    );

    let output = run.output();
    assert_eq!(output.state, json!({"last_id": 102}));
    assert_eq!(run.calls.len(), 2, "{:?}", run.calls);
    for (call, key) in run.calls.iter().zip(["ABC-101", "ABC-102"]) {
        assert_eq!(call[..3], ["task", "create", "--data"]);
        let data: Value = serde_json::from_str(&call[3]).expect("the data is JSON");
        assert_eq!(
            data,
            json!({"key": key, "summary": format!("Summary of {key}")})
        );
    }
    assert!(
        !run.calls.concat().concat().contains("Kowalski"),
        "no personal data leaves the script"
    );
}

/// A task that is not created stops the run's creates; the state covers the
/// tasks created before it, so the next run repeats none of them.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn jira_stops_at_a_task_not_created_and_keeps_the_state_of_the_tasks_before_it() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "search",
        &json!({"issues": [
            jira_issue(101, "ABC-101"),
            jira_issue(102, "ABC-102"),
            jira_issue(103, "ABC-103"),
        ]}),
    );
    sandbox.fail_from(2);

    let run = sandbox.run(
        JIRA,
        HookPhase::Check,
        &input(jira_condition(), json!({"last_id": 100}), Value::Null),
    );

    let output = run.output();
    assert_eq!(output.state, json!({"last_id": 101}));
    assert_eq!(
        run.calls.len(),
        2,
        "no create after the refused one: {:?}",
        run.calls
    );
    assert!(run.stderr.contains("ABC-102"), "{}", run.stderr);
}

/// A run in which no task is created fails: the error is recorded and
/// counted, so a refusal that does not go away brings the observer to
/// `error`, not to the same issue on every run with no trace.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn jira_fails_the_run_when_no_task_is_created() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "search",
        &json!({"issues": [
            jira_issue(101, "ABC-101"),
            jira_issue(102, "ABC-102"),
        ]}),
    );
    sandbox.fail_from(1);

    let run = sandbox.run(
        JIRA,
        HookPhase::Check,
        &input(jira_condition(), json!({"last_id": 100}), Value::Null),
    );

    assert_eq!(run.code, Some(1), "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "no state: {:?}", run.stdout);
    assert_eq!(
        run.calls.len(),
        1,
        "no create after the refused one: {:?}",
        run.calls
    );
    assert!(
        run.stderr.contains("the task of ABC-101 was not created"),
        "{}",
        run.stderr
    );
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn jira_keeps_the_old_state_when_the_answer_is_not_a_list_of_issues() {
    let sandbox = Sandbox::new();
    sandbox.answer("search", &json!({"errorMessages": ["no"]}));

    let run = sandbox.run(
        JIRA,
        HookPhase::Check,
        &input(jira_condition(), json!({"last_id": 100}), Value::Null),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(run.calls.is_empty());
}

const JIRA: &str = "jira-new-issues";
const GITLAB: &str = "gitlab-mr-label";
const GITHUB: &str = "github-pr-label";

const DONE: Option<i32> = Some(0);
const ERROR: Option<i32> = Some(2);

const GITLAB_MR: &str = "https://gitlab.example.com/group/app/-/merge_requests/7";
const GITHUB_PR: &str = "https://github.com/acme/app/pull/17";

/// When the gate of the entries below began.
const STARTED: &str = "2026-11-17T10:00:00Z";

/// The queue entry of a review gate (`review_work`) for `url`, as the
/// control plane hands it to the script (contract version 3), with the
/// param `mr_url` and the `extra` ones.
fn review_entry(url: &str, extra: &[(&str, &str)]) -> Value {
    let mut params: BTreeMap<String, String> = [("mr_url".to_string(), url.to_string())]
        .into_iter()
        .collect();
    for (name, value) in extra {
        params.insert(name.to_string(), value.to_string());
    }
    let choice = |name: &str, requires: &[&str]| ScriptChoice {
        name: name.into(),
        description: None,
        requires_variables: requires.iter().map(|name| name.to_string()).collect(),
    };
    serde_json::to_value(ScriptEntry {
        task_id: TaskId::parse("task-7").unwrap(),
        episode: "input-3".into(),
        episode_started_at: STARTED.parse().unwrap(),
        step: "review_work".into(),
        params,
        choices: vec![
            choice("check_feedback", &[]),
            choice("revise_plan", &["note"]),
            choice("abandon", &[]),
        ],
        variables: vec![ScriptVariable {
            name: "note".into(),
            variable_type: Some("text".into()),
            description: None,
            options: Vec::new(),
        }],
    })
    .unwrap()
}

/// The input of an `on_episodes` run for `entry` (`Value::Null`: an
/// `always` run, with no entry).
fn review_input(condition: Value, entry: Value) -> Value {
    input(condition, Value::Null, entry)
}

/// The condition of the review queue: the label `w CR` (with a space, as
/// a label can have) and the GitLab host; GitHub takes its defaults.
fn review_condition() -> Value {
    json!({"label": "w CR", "gitlab": {"host": "gitlab.example.com"}})
}

fn gitlab_input(entry: Value) -> Value {
    review_input(review_condition(), entry)
}

fn github_input(entry: Value) -> Value {
    review_input(review_condition(), entry)
}

/// The input of the `phase` script of the review queue for `entry`; an
/// `out` run also gets how the gate was left.
fn hook_input(phase: HookPhase, entry: Value) -> Value {
    let mut input = review_input(review_condition(), entry);
    input["phase"] = json!(phase.as_str());
    if phase == HookPhase::Out {
        input["exit"] = json!({"choice": "check_feedback", "author": "person"});
    }
    input
}

/// A GitLab merge request as the REST API gives it.
fn merge_request(state: &str, labels: &[&str]) -> Value {
    json!({"iid": 7, "state": state, "labels": labels})
}

/// A GitHub pull request as the REST API gives it.
fn pull_request(state: &str, labels: &[&str]) -> Value {
    let labels: Vec<Value> = labels
        .iter()
        .map(|name| json!({"id": 1, "name": name}))
        .collect();
    json!({"number": 17, "state": state, "merged": false, "merged_at": null, "labels": labels})
}

/// Asserts a review run's exit code and that it printed nothing on stdout.
fn assert_exit(run: &Run, code: Option<i32>) {
    assert_eq!(run.code, code, "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
}

/// The `narsor-cli` call that answers the gate of `review_entry`.
fn decide_call(choice: &str, url: &str) -> Vec<String> {
    [
        "task",
        "decide",
        "task-7",
        "--episode",
        "input-3",
        "--choice",
        choice,
        "--reason",
        &format!("the label w CR was removed from {url}"),
    ]
    .map(str::to_string)
    .to_vec()
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_answers_check_feedback_when_the_reviewer_removed_the_label() {
    // Covers AE1: the reviewer removed the label; the script answers the
    // gate with `task decide`, with no variables and no artifacts.
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &["backend"]));

    let run = sandbox.run(
        GITLAB,
        HookPhase::Check,
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );

    assert_exit(&run, DONE);
    assert_eq!(run.calls, vec![decide_call("check_feedback", GITLAB_MR)]);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_answers_the_choice_of_its_param_when_the_reviewer_removed_the_label() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &pull_request("open", &[]));

    let run = sandbox.run(
        GITHUB,
        HookPhase::Check,
        &github_input(review_entry(GITHUB_PR, &[("choice", "abandon")])),
    );

    assert_exit(&run, DONE);
    assert_eq!(run.calls, vec![decide_call("abandon", GITHUB_PR)]);
}

/// R13: `in` adds the label only when it is missing and `out` removes it
/// only when it is there, so a retry or a label the reviewer already
/// removed changes nothing. Both automations, both phases.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn entry_and_exit_scripts_change_the_label_only_when_needed() {
    let gitlab_path = "projects/group%2Fapp/merge_requests/7";
    let github_path = "repos/acme/app/issues/17";
    for (automation, url, source, with_label, without_label, add, remove) in [
        (
            GITLAB,
            GITLAB_MR,
            "mr-group-app-7",
            merge_request("opened", &["backend", "w CR"]),
            merge_request("opened", &["backend"]),
            format!("PUT {gitlab_path} add_labels=w CR"),
            format!("PUT {gitlab_path} remove_labels=w CR"),
        ),
        (
            GITHUB,
            GITHUB_PR,
            "pr-acme-app-17",
            pull_request("open", &["w CR"]),
            pull_request("open", &[]),
            format!(r#"POST {github_path}/labels {{"labels":["w CR"]}}"#),
            format!("DELETE {github_path}/labels/w%20CR "),
        ),
    ] {
        for (phase, request, expected) in [
            (HookPhase::In, &without_label, Some(add.as_str())),
            (HookPhase::In, &with_label, None),
            (HookPhase::Out, &with_label, Some(remove.as_str())),
            (HookPhase::Out, &without_label, None),
        ] {
            let sandbox = Sandbox::new();
            sandbox.answer(source, request);

            let run = sandbox.run(
                automation,
                phase,
                &hook_input(phase, review_entry(url, &[])),
            );

            assert_exit(&run, DONE);
            assert_eq!(
                sandbox.writes(),
                expected.map(str::to_string).into_iter().collect::<Vec<_>>(),
                "{automation} {phase}"
            );
            assert!(
                run.calls.is_empty(),
                "{automation} {phase}: {:?}",
                run.calls
            );
        }
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn a_refused_label_change_is_an_error_of_the_entry_or_exit_script() {
    // A failed attempt: Narsor retries it, and after the last one a failed
    // `in` fails the gate step and a failed `out` is logged.
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[]));
    sandbox.answer("pr-acme-app-17", &pull_request("open", &["w CR"]));
    fs::write(sandbox.path.join("source/writes.status"), "403").unwrap();

    let entry = sandbox.run(
        GITLAB,
        HookPhase::In,
        &hook_input(HookPhase::In, review_entry(GITLAB_MR, &[])),
    );
    let exit = sandbox.run(
        GITHUB,
        HookPhase::Out,
        &hook_input(HookPhase::Out, review_entry(GITHUB_PR, &[])),
    );

    for run in [&entry, &exit] {
        assert_exit(run, ERROR);
        assert!(run.stderr.contains("HTTP 403"), "{}", run.stderr);
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn two_example_automations_of_one_queue_handle_only_their_own_urls() {
    // Covers AE4 of the queue: the GitLab and the GitHub automation serve
    // the queue `mr-review`. For each entry only the scripts of its host
    // act; the others exit 0 with no answer and no change, and fetch
    // nothing (no fake answer exists for them, so a fetch would fail the
    // run).
    for (url, owner, other) in [(GITHUB_PR, GITHUB, GITLAB), (GITLAB_MR, GITLAB, GITHUB)] {
        let sandbox = Sandbox::new();
        sandbox.answer("pr-acme-app-17", &pull_request("open", &[]));
        sandbox.answer("mr-group-app-7", &merge_request("opened", &[]));

        for phase in HookPhase::ALL {
            let skipped = sandbox.run(other, phase, &hook_input(phase, review_entry(url, &[])));
            assert_exit(&skipped, DONE);
            assert!(skipped.calls.is_empty(), "{other}: {:?}", skipped.calls);
            assert!(skipped.stderr.contains("not mine"), "{}", skipped.stderr);
        }
        assert!(sandbox.writes().is_empty(), "{:?}", sandbox.writes());

        let answered = sandbox.run(
            owner,
            HookPhase::Check,
            &hook_input(HookPhase::Check, review_entry(url, &[])),
        );
        assert_exit(&answered, DONE);
        assert_eq!(
            answered.calls,
            vec![decide_call("check_feedback", url)],
            "{owner}"
        );
    }
}

/// `mr_url` comes from the workflow (an agent can write it), so with an
/// allowlist in the condition a URL of another project on the same host is
/// not the automation's entry: no fetch, no write, no answer, in any phase.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn a_url_outside_the_allowlist_is_not_touched() {
    let condition = json!({
        "label": "w CR",
        "gitlab": {"host": "gitlab.example.com", "projects": ["group/other"]},
        "github": {"repos": ["acme/other"]},
    });
    for (automation, url) in [(GITLAB, GITLAB_MR), (GITHUB, GITHUB_PR)] {
        let sandbox = Sandbox::new();
        for phase in HookPhase::ALL {
            let mut input = review_input(condition.clone(), review_entry(url, &[]));
            input["phase"] = json!(phase.as_str());

            let run = sandbox.run(automation, phase, &input);

            assert_exit(&run, DONE);
            assert!(
                run.calls.is_empty(),
                "{automation} {phase}: {:?}",
                run.calls
            );
            assert!(run.stderr.contains("not mine"), "{}", run.stderr);
        }
        assert!(sandbox.writes().is_empty(), "{:?}", sandbox.writes());
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn no_answer_while_the_label_is_on_or_the_request_is_not_open() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "mr-group-app-7",
        &merge_request("opened", &["backend", "w CR"]),
    );
    sandbox.answer("pr-acme-app-17", &pull_request("closed", &[]));

    let gitlab = sandbox.run(
        GITLAB,
        HookPhase::Check,
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&gitlab, DONE);
    assert!(
        gitlab.stderr.contains("still has the label"),
        "{}",
        gitlab.stderr
    );

    let github = sandbox.run(
        GITHUB,
        HookPhase::Check,
        &github_input(review_entry(GITHUB_PR, &[])),
    );
    assert_exit(&github, DONE);
    assert!(github.stderr.contains("is closed"), "{}", github.stderr);

    // GitHub matches label names without case: `W cr` is the label `w CR`,
    // so it must not look like the reviewer removed it.
    sandbox.answer("pr-acme-app-17", &pull_request("open", &["W cr"]));
    let other_case = sandbox.run(
        GITHUB,
        HookPhase::Check,
        &github_input(review_entry(GITHUB_PR, &[])),
    );
    assert_exit(&other_case, DONE);
    assert!(other_case.calls.is_empty(), "{:?}", other_case.calls);

    sandbox.answer("mr-group-app-7", &merge_request("merged", &[]));
    let merged = sandbox.run(
        GITLAB,
        HookPhase::Check,
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&merged, DONE);
    assert!(merged.calls.is_empty(), "{:?}", merged.calls);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_does_not_fetch_a_url_that_is_not_a_merge_request_on_its_host() {
    let sandbox = Sandbox::new();
    for url in [
        "https://gitlab.example.com/group/app/-/merge_requests/6/diffs",
        "https://gitlab.example.com/group/app/-/merge_requests/5/",
        "https://evil.example.com/group/app/-/merge_requests/7",
        "",
    ] {
        let run = sandbox.run(
            GITLAB,
            HookPhase::Check,
            &gitlab_input(review_entry(url, &[])),
        );
        assert_exit(&run, DONE);
        assert!(
            run.stderr.contains("not mine, not fetched"),
            "{}",
            run.stderr
        );
        assert!(run.calls.is_empty(), "{:?}", run.calls);
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn a_bad_input_condition_or_param_is_an_error_of_the_run() {
    // Any exit code but 0 is an error, recorded for the entry.
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[]));
    sandbox.answer("pr-acme-app-17", &pull_request("open", &[]));
    let mut version_one = gitlab_input(review_entry(GITLAB_MR, &[]));
    version_one["contract_version"] = json!(1);
    let mut no_host = gitlab_input(review_entry(GITLAB_MR, &[]));
    no_host["condition"]
        .as_object_mut()
        .unwrap()
        .remove("gitlab");
    let in_input_to_check = hook_input(HookPhase::In, review_entry(GITLAB_MR, &[]));

    for (automation, input, reason) in [
        (GITLAB, gitlab_input(Value::Null), "no queue entry"),
        (GITHUB, github_input(Value::Null), "no queue entry"),
        (GITLAB, version_one, "not version 3"),
        (GITLAB, in_input_to_check, "for the phase check"),
        (GITLAB, no_host, "condition.gitlab.host"),
        (
            GITHUB,
            github_input(review_entry(GITHUB_PR, &[("choice", "approve")])),
            "has no choice approve",
        ),
    ] {
        let run = sandbox.run(automation, HookPhase::Check, &input);
        assert_exit(&run, ERROR);
        assert!(run.stderr.contains(reason), "{automation}: {}", run.stderr);
        assert!(run.calls.is_empty(), "{:?}", run.calls);
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn a_source_that_fails_or_a_refused_answer_is_an_error_of_the_run() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "pr-acme-app-17",
        &json!({"message": "API rate limit exceeded for user ID 1."}),
    );
    sandbox.status("pr-acme-app-17", 403);
    let run = sandbox.run(
        GITHUB,
        HookPhase::Check,
        &github_input(review_entry(GITHUB_PR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.stderr.contains("HTTP 403"), "{}", run.stderr);
    assert!(run.calls.is_empty());

    sandbox.status("mr-group-app-7", 404);
    let run = sandbox.run(
        GITLAB,
        HookPhase::Check,
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.calls.is_empty());

    // A stale episode or another answer first: narsor-cli exits 1.
    fs::remove_file(sandbox.path.join("source/mr-group-app-7.status")).unwrap();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[]));
    sandbox.fail_from(1);
    let run = sandbox.run(
        GITLAB,
        HookPhase::Check,
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.stderr.contains("was refused"), "{}", run.stderr);
    assert_eq!(run.calls.len(), 1);
}
