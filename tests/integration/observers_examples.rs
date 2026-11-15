//! The example observer scripts (U8) against a fake source: each one reads
//! the answers of its source from `FAKE_SOURCE_DIR` and calls a stand-in
//! `narsor-cli` that records its arguments. The Jira script must print a
//! document that the script contract accepts; a PR/MR script checks one
//! watch entry and answers with its exit code only (D18).
//!
//! These tests run the real scripts with `sh` and `jq`, so they are
//! `#[ignore]`d: `cargo test -- --include-ignored` runs them.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use narsor_engine::observers::{parse_script_output, ScriptOutput};
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
                 if [ -f '{reply}' ]; then cat '{reply}'; else echo 'delivered to the waiting step'; fi\n",
                log = path.join("calls").display(),
                count = path.join("count").display(),
                fail = path.join("fail_from").display(),
                reply = path.join("reply").display(),
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

    /// The stand-in `narsor-cli` answers `line` on stdout, as the server's
    /// line of the effect.
    fn reply(&self, line: &str) {
        fs::write(self.path.join("reply"), format!("{line}\n")).expect("write reply");
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

fn input(source: &str, condition: Value, state: Value, watch: Value) -> Value {
    json!({
        "contract_version": 1,
        "observer": {"id": "obs-1", "name": "example", "source": source},
        "condition": condition,
        "state": state,
        "watch": watch,
        "first_run": state.is_null(),
        "import_backlog": false,
    })
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
        &input("jira", jira_condition(), Value::Null, Value::Null),
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
        &input(
            "jira",
            jira_condition(),
            json!({"last_id": 100}),
            Value::Null,
        ),
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
        &input(
            "jira",
            jira_condition(),
            json!({"last_id": 100}),
            Value::Null,
        ),
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
        &input(
            "jira",
            jira_condition(),
            json!({"last_id": 100}),
            Value::Null,
        ),
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
        &input(
            "jira",
            jira_condition(),
            json!({"last_id": 100}),
            Value::Null,
        ),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(run.calls.is_empty());
}

const DONE: Option<i32> = Some(0);
const AGAIN: Option<i32> = Some(1);
const ERROR: Option<i32> = Some(2);

fn pr_watch(number: u32, types: &[&str], episode: Option<&str>) -> Value {
    json!({"object": format!("https://github.com/acme/app/pull/{number}"),
           "task_id": format!("task-{number}"),
           "event_types": types, "wait_episode": episode})
}

fn mr_watch(object: &str, types: &[&str], episode: Option<&str>) -> Value {
    json!({"object": object, "task_id": "task-7", "event_types": types, "wait_episode": episode})
}

fn gitlab_input(watch: Value) -> Value {
    input(
        "gitlab.example.com",
        json!({"host": "gitlab.example.com"}),
        Value::Null,
        watch,
    )
}

/// Asserts a watch run's exit code and that it printed nothing on stdout.
fn assert_exit(run: &Run, code: Option<i32>) {
    assert_eq!(run.code, code, "stderr: {}", run.stderr);
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_0_after_a_review_event_reaches_the_waiting_step() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &json!({"state": "open", "merged": false}));
    sandbox.answer(
        "reviews-acme-app-17",
        &json!([{"state": "COMMENTED"}, {"state": "CHANGES_REQUESTED"}, {"state": "APPROVED"}]),
    );

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(17, &["review.approved", "merged"], Some("ep-1")),
        ),
    );

    assert_exit(&run, DONE);
    assert_eq!(run.calls.len(), 1, "{:?}", run.calls);
    let call = &run.calls[0];
    assert_eq!(
        call[..6],
        [
            "task",
            "event",
            "task-17",
            "--type",
            "review.approved",
            "--data"
        ]
    );
    let data: Value = serde_json::from_str(&call[6]).expect("the data is JSON");
    assert_eq!(
        data,
        json!({"pr_url": "https://github.com/acme/app/pull/17", "number": 17})
    );
}

/// Q18: an event the task does not wait for is only recorded; the entry
/// stays, so a later approval still reaches the task.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_1_when_the_review_event_is_only_recorded() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &json!({"state": "open", "merged": false}));
    sandbox.answer(
        "reviews-acme-app-17",
        &json!([{"state": "CHANGES_REQUESTED"}]),
    );
    sandbox.reply("recorded in the history of task task-17; it does not wait for it");

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(
                17,
                &["review.approved", "review.changes_requested"],
                Some("ep-1"),
            ),
        ),
    );

    assert_exit(&run, AGAIN);
    assert_eq!(run.calls.len(), 1, "{:?}", run.calls);
    assert_eq!(run.calls[0][4], "review.changes_requested");
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_sends_nothing_to_a_task_that_does_not_wait_and_exits_1() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-18", &json!({"state": "open", "merged": false}));
    sandbox.answer("reviews-acme-app-18", &json!([{"state": "APPROVED"}]));

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(18, &["review.approved"], None),
        ),
    );

    assert_exit(&run, AGAIN);
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_0_for_a_merged_pull_request_even_when_the_task_does_not_wait() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "pr-acme-app-18",
        &json!({"state": "closed", "merged": true, "merged_at": "2026-11-14T10:00:00Z"}),
    );
    sandbox.answer("reviews-acme-app-18", &json!([]));

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(18, &["review.approved", "merged"], None),
        ),
    );

    assert_exit(&run, DONE);
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_0_without_the_token_for_an_object_on_another_host_or_gone() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-19", &json!({"message": "Not Found"}));
    sandbox.status("pr-acme-app-19", 404);
    let evil = json!({"object": "https://evil.example.com/acme/app/pull/1", "task_id": "task-1",
                      "event_types": ["review.approved"], "wait_episode": "ep-2"});

    let run = sandbox.run(
        "github-pr-review.sh",
        &input("github.com", json!({}), Value::Null, evil),
    );
    assert_exit(&run, DONE);
    assert!(run.stderr.contains("evil.example.com"), "{}", run.stderr);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(19, &["review.approved"], Some("ep-3")),
        ),
    );
    assert_exit(&run, DONE);
    assert!(run.stderr.contains("pull/19"), "{}", run.stderr);
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_2_at_a_rate_limit_and_without_a_watch_entry() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "pr-acme-app-17",
        &json!({"message": "API rate limit exceeded for user ID 1."}),
    );
    sandbox.status("pr-acme-app-17", 403);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(17, &["review.approved"], Some("ep-1")),
        ),
    );
    assert_exit(&run, ERROR);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input("github.com", json!({}), Value::Null, Value::Null),
    );
    assert_exit(&run, ERROR);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_exits_2_when_the_event_is_refused() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &json!({"state": "open"}));
    sandbox.answer("reviews-acme-app-17", &json!([{"state": "APPROVED"}]));
    sandbox.fail_from(1);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input(
            "github.com",
            json!({}),
            Value::Null,
            pr_watch(17, &["review.approved"], Some("ep-1")),
        ),
    );

    assert_exit(&run, ERROR);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_exits_0_for_a_bad_or_forbidden_object_without_fetching_it() {
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-8", &json!({"message": "403 Forbidden"}));
    sandbox.status("mr-group-app-8", 403);
    for object in [
        "https://gitlab.example.com/group/app/-/merge_requests/6/diffs",
        "https://gitlab.example.com/group/app/-/merge_requests/5/",
        "https://gitlab.example.com/group/app/-/merge_requests/8",
    ] {
        let run = sandbox.run(
            "gitlab-mr-review.sh",
            &gitlab_input(mr_watch(object, &["review.approved"], Some("ep-1"))),
        );
        assert_exit(&run, DONE);
    }
    assert!(sandbox
        .run(
            "gitlab-mr-review.sh",
            &gitlab_input(mr_watch(
                "https://gitlab.example.com/group/app/-/merge_requests/6/diffs",
                &["review.approved"],
                Some("ep-1")
            ))
        )
        .stderr
        .contains("/diffs"));
    assert!(sandbox
        .run("gitlab-mr-review.sh", &gitlab_input(Value::Null))
        .calls
        .is_empty());
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_exits_2_when_the_source_fails() {
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &json!({"message": "oops"}));
    sandbox.status("mr-group-app-7", 500);

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(mr_watch(
            "https://gitlab.example.com/group/app/-/merge_requests/7",
            &["review.approved"],
            Some("ep-1"),
        )),
    );

    assert_exit(&run, ERROR);
    assert!(run.calls.is_empty());
}

/// One event per wait: after the review event reaches the step the script
/// exits 0, and the merge of the same run is not sent.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_delivers_the_review_event_once_and_exits_0() {
    let sandbox = Sandbox::new();
    let name = "group-app-mr-7";
    sandbox.answer(&format!("mr-{name}"), &json!({"state": "merged"}));
    sandbox.answer(&format!("approvals-{name}"), &json!({"approved": true}));
    sandbox.answer(
        &format!("reviewers-{name}"),
        &json!([{"state": "approved"}]),
    );

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(mr_watch(
            "https://gitlab.example.com/group/app-mr/-/merge_requests/7",
            &["review.approved", "merged"],
            Some("ep-3"),
        )),
    );

    assert_exit(&run, DONE);
    let types: Vec<&str> = run.calls.iter().map(|call| call[4].as_str()).collect();
    assert_eq!(types, ["review.approved"]);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_exits_1_while_the_merge_request_waits_for_a_review() {
    let sandbox = Sandbox::new();
    let name = "group-app-7";
    sandbox.answer(&format!("mr-{name}"), &json!({"state": "opened"}));
    sandbox.answer(&format!("approvals-{name}"), &json!({"approved": false}));
    sandbox.answer(&format!("reviewers-{name}"), &json!([]));

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(mr_watch(
            "https://gitlab.example.com/group/app/-/merge_requests/7",
            &["review.approved"],
            Some("ep-3"),
        )),
    );

    assert_exit(&run, AGAIN);
    assert!(run.calls.is_empty());
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_delivers_a_merge_without_a_review_and_exits_0() {
    let sandbox = Sandbox::new();
    let name = "group-app-7";
    sandbox.answer(&format!("mr-{name}"), &json!({"state": "merged"}));
    sandbox.answer(&format!("approvals-{name}"), &json!({"approved": false}));
    sandbox.answer(&format!("reviewers-{name}"), &json!([]));

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &gitlab_input(mr_watch(
            "https://gitlab.example.com/group/app/-/merge_requests/7",
            &["review.approved", "merged"],
            Some("ep-3"),
        )),
    );

    assert_exit(&run, DONE);
    let types: Vec<&str> = run.calls.iter().map(|call| call[4].as_str()).collect();
    assert_eq!(types, ["merged"]);
}
