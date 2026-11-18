//! The example observer scripts against a fake source: each one reads the
//! answers of its source from `FAKE_SOURCE_DIR` and calls a stand-in
//! `narsor-cli` that records its arguments. The input follows version 2 of
//! the script contract. The Jira script (`always`) must print a document
//! that the contract accepts; a PR/MR script (`on_episodes`) checks one
//! queue entry and answers its gate with `narsor-cli task decide` (U13).
//!
//! These tests run the real scripts with `sh` and `jq`, so they are
//! `#[ignore]`d: `cargo test -- --include-ignored` runs them.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use narsor_engine::domain::TaskId;
use narsor_engine::observers::{
    parse_script_output, ScriptChoice, ScriptEntry, ScriptOutput, ScriptVariable,
    SCRIPT_CONTRACT_VERSION,
};
use serde_json::{json, Value};

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

    fn run(&self, example: &str, input: &Value) -> Run {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("workflows/observers/examples")
            .join(example);
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

/// The script input; `entry` is `Value::Null` for an `always` run, which
/// has no `entry` field.
fn input(condition: Value, state: Value, entry: Value) -> Value {
    let mut input = json!({
        "contract_version": SCRIPT_CONTRACT_VERSION,
        "observer": {"id": "obs-1", "name": "example"},
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
        "jira-new-issues.sh",
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
        "jira-new-issues.sh",
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
        "jira-new-issues.sh",
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
        "jira-new-issues.sh",
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
        "jira-new-issues.sh",
        &input(jira_condition(), json!({"last_id": 100}), Value::Null),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(run.calls.is_empty());
}

const DONE: Option<i32> = Some(0);
const ERROR: Option<i32> = Some(2);

const GITLAB_MR: &str = "https://gitlab.example.com/group/app/-/merge_requests/7";
const GITHUB_PR: &str = "https://github.com/acme/app/pull/17";

/// When the gate of the entries below began.
const STARTED: &str = "2026-11-17T10:00:00Z";
/// A change of the pull or merge request after `STARTED`.
const LATER: &str = "2026-11-17T10:20:00.123Z";
/// A change before `STARTED`.
const EARLIER: &str = "2026-11-17T09:50:00Z";

/// The queue entry of a review gate (`review_work`) for `url`, as the
/// control plane hands it to the script (contract version 2), with the
/// params `mr_url`, `label` and the `extra` ones.
fn review_entry(url: &str, extra: &[(&str, &str)]) -> Value {
    let mut params: BTreeMap<String, String> = [
        ("mr_url".to_string(), url.to_string()),
        ("label".to_string(), "w CR".to_string()),
    ]
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

fn gitlab_input(entry: Value) -> Value {
    review_input(json!({"host": "gitlab.example.com"}), entry)
}

fn github_input(entry: Value) -> Value {
    review_input(json!({}), entry)
}

/// A GitLab merge request as the REST API gives it.
fn merge_request(state: &str, labels: &[&str], updated_at: &str) -> Value {
    json!({"iid": 7, "state": state, "labels": labels, "updated_at": updated_at})
}

/// A GitHub pull request as the REST API gives it.
fn pull_request(state: &str, labels: &[&str], updated_at: &str) -> Value {
    let labels: Vec<Value> = labels
        .iter()
        .map(|name| json!({"id": 1, "name": name}))
        .collect();
    json!({"number": 17, "state": state, "merged": false, "merged_at": null,
           "labels": labels, "updated_at": updated_at})
}

/// Asserts a review run's exit code and that it printed nothing on stdout.
fn assert_exit(run: &Run, code: Option<i32>) {
    assert_eq!(run.code, code, "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
}

/// The `narsor-cli` call that answers the gate of `review_entry`.
fn decide_call(choice: &str, label: &str, url: &str) -> Vec<String> {
    [
        "task",
        "decide",
        "task-7",
        "--episode",
        "input-3",
        "--choice",
        choice,
        "--reason",
        &format!("the label {label} was removed from {url}"),
    ]
    .map(str::to_string)
    .to_vec()
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_answers_check_feedback_when_the_reviewer_removed_the_label() {
    // Covers AE1: the reviewer removed the label; the script answers the
    // gate with `task decide`, with no variables and no artifacts (R12).
    let sandbox = Sandbox::new();
    sandbox.answer(
        "mr-group-app-7",
        &merge_request("opened", &["backend"], LATER),
    );

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );

    assert_exit(&run, DONE);
    assert_eq!(
        run.calls,
        vec![decide_call("check_feedback", "w CR", GITLAB_MR)]
    );
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_answers_the_choice_of_its_param_when_the_reviewer_removed_the_label() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &pull_request("open", &[], LATER));

    let run = sandbox.run(
        "github-pr-review.sh",
        &github_input(review_entry(GITHUB_PR, &[("choice", "abandon")])),
    );

    assert_exit(&run, DONE);
    assert_eq!(run.calls, vec![decide_call("abandon", "w CR", GITHUB_PR)]);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn two_example_observers_of_one_queue_answer_only_their_own_urls() {
    // Covers AE4: the GitLab and the GitHub observer serve the queue
    // `mr-review`. For each entry only the script of its host answers; the
    // other exits 0 with no answer and fetches nothing (no fake answer
    // exists for it, so a fetch would fail the run).
    for (url, owner, other) in [
        (GITHUB_PR, "github-pr-review.sh", "gitlab-mr-review.sh"),
        (GITLAB_MR, "gitlab-mr-review.sh", "github-pr-review.sh"),
    ] {
        let sandbox = Sandbox::new();
        sandbox.answer("pr-acme-app-17", &pull_request("open", &[], LATER));
        sandbox.answer("mr-group-app-7", &merge_request("opened", &[], LATER));
        let input_for = |script: &str| {
            let entry = review_entry(url, &[]);
            if script.starts_with("gitlab") {
                gitlab_input(entry)
            } else {
                github_input(entry)
            }
        };

        let skipped = sandbox.run(other, &input_for(other));
        assert_exit(&skipped, DONE);
        assert!(skipped.calls.is_empty(), "{other}: {:?}", skipped.calls);
        assert!(skipped.stderr.contains("not mine"), "{}", skipped.stderr);

        let answered = sandbox.run(owner, &input_for(owner));
        assert_exit(&answered, DONE);
        assert_eq!(
            answered.calls,
            vec![decide_call("check_feedback", "w CR", url)],
            "{owner}"
        );
    }
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn no_answer_while_the_label_is_on_or_the_request_is_not_open() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "mr-group-app-7",
        &merge_request("opened", &["backend", "w CR"], LATER),
    );
    sandbox.answer("pr-acme-app-17", &pull_request("closed", &[], LATER));

    let gitlab = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&gitlab, DONE);
    assert!(
        gitlab.stderr.contains("still has the label"),
        "{}",
        gitlab.stderr
    );

    let github = sandbox.run(
        "github-pr-review.sh",
        &github_input(review_entry(GITHUB_PR, &[])),
    );
    assert_exit(&github, DONE);
    assert!(github.stderr.contains("is closed"), "{}", github.stderr);

    sandbox.answer("mr-group-app-7", &merge_request("merged", &[], LATER));
    let merged = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&merged, DONE);
    assert!(sandbox
        .run(
            "gitlab-mr-review.sh",
            &gitlab_input(review_entry(GITLAB_MR, &[]))
        )
        .calls
        .is_empty());
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn no_answer_for_a_label_that_was_gone_before_the_gate_started() {
    // The workflow did not put the label on the request: nothing changed
    // since the gate started, so the script does not answer at once.
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[], EARLIER));
    sandbox.answer("pr-acme-app-17", &pull_request("open", &[], EARLIER));

    for (script, input) in [
        (
            "gitlab-mr-review.sh",
            gitlab_input(review_entry(GITLAB_MR, &[])),
        ),
        (
            "github-pr-review.sh",
            github_input(review_entry(GITHUB_PR, &[])),
        ),
    ] {
        let run = sandbox.run(script, &input);
        assert_exit(&run, DONE);
        assert!(run.calls.is_empty(), "{script}: {:?}", run.calls);
        assert!(run.stderr.contains("did not change"), "{}", run.stderr);
    }
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
        let run = sandbox.run("gitlab-mr-review.sh", &gitlab_input(review_entry(url, &[])));
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
fn a_bad_input_or_param_is_an_error_of_the_run() {
    // R14: any exit code but 0 is an error, recorded for the entry.
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[], LATER));
    sandbox.answer("pr-acme-app-17", &pull_request("open", &[], LATER));
    let mut no_label = review_entry(GITLAB_MR, &[]);
    no_label["params"].as_object_mut().unwrap().remove("label");
    let mut version_one = gitlab_input(review_entry(GITLAB_MR, &[]));
    version_one["contract_version"] = json!(1);

    for (script, input, reason) in [
        (
            "gitlab-mr-review.sh",
            gitlab_input(Value::Null),
            "no queue entry",
        ),
        (
            "github-pr-review.sh",
            github_input(Value::Null),
            "no queue entry",
        ),
        ("gitlab-mr-review.sh", version_one, "not version 2"),
        (
            "gitlab-mr-review.sh",
            gitlab_input(no_label),
            "no param label",
        ),
        (
            "github-pr-review.sh",
            github_input(review_entry(GITHUB_PR, &[("choice", "approve")])),
            "has no choice approve",
        ),
    ] {
        let run = sandbox.run(script, &input);
        assert_exit(&run, ERROR);
        assert!(run.stderr.contains(reason), "{script}: {}", run.stderr);
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
        "github-pr-review.sh",
        &github_input(review_entry(GITHUB_PR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.stderr.contains("HTTP 403"), "{}", run.stderr);
    assert!(run.calls.is_empty());

    sandbox.status("mr-group-app-7", 404);
    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.calls.is_empty());

    // A stale episode or another answer first: narsor-cli exits 1.
    fs::remove_file(sandbox.path.join("source/mr-group-app-7.status")).unwrap();
    sandbox.answer("mr-group-app-7", &merge_request("opened", &[], LATER));
    sandbox.fail_from(1);
    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(review_entry(GITLAB_MR, &[])),
    );
    assert_exit(&run, ERROR);
    assert!(run.stderr.contains("was refused"), "{}", run.stderr);
    assert_eq!(run.calls.len(), 1);
}
