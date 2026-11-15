//! The example observer scripts (U8) against a fake source: each one reads
//! the answers of its source from `FAKE_SOURCE_DIR`, calls a stand-in
//! `narsor-cli` that records its arguments, and must print a document that
//! the script contract accepts.
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
                 echo 'created' >&2\n",
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

fn input(source: &str, condition: Value, state: Value, watches: Value) -> Value {
    json!({
        "contract_version": 1,
        "observer": {"id": "obs-1", "name": "example", "source": source},
        "condition": condition,
        "state": state,
        "watches": watches,
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
        &input("jira", jira_condition(), Value::Null, json!([])),
    );

    let output = run.output();
    assert_eq!(output.state, json!({"last_id": 105}));
    assert!(output.closed_watches.is_empty());
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
        &input("jira", jira_condition(), json!({"last_id": 100}), json!([])),
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
        &input("jira", jira_condition(), json!({"last_id": 100}), json!([])),
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
        &input("jira", jira_condition(), json!({"last_id": 100}), json!([])),
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
        &input("jira", jira_condition(), json!({"last_id": 100}), json!([])),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(run.calls.is_empty());
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_delivers_events_to_waiting_tasks_and_closes_their_entries() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &json!({"state": "open", "merged": false}));
    sandbox.answer(
        "reviews-acme-app-17",
        &json!([{"state": "COMMENTED"}, {"state": "CHANGES_REQUESTED"}, {"state": "APPROVED"}]),
    );
    sandbox.answer(
        "pr-acme-app-18",
        &json!({"state": "closed", "merged": true, "merged_at": "2026-11-14T10:00:00Z"}),
    );
    sandbox.answer("reviews-acme-app-18", &json!([{"state": "APPROVED"}]));
    let watches = json!([
        {"object": "https://github.com/acme/app/pull/17", "task_id": "task-17",
         "event_types": ["review.approved", "merged"], "wait_episode": "ep-1"},
        {"object": "https://github.com/acme/app/pull/18", "task_id": "task-18",
         "event_types": ["review.approved", "merged"], "wait_episode": null},
    ]);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input("github.com", json!({}), json!({"seen": 1}), watches),
    );

    let output = run.output();
    assert_eq!(output.state, json!({"seen": 1}), "the state is kept");
    assert_eq!(
        output.closed_watches,
        [
            "https://github.com/acme/app/pull/17",
            "https://github.com/acme/app/pull/18"
        ],
        "an entry is closed after its review event, and when merged"
    );
    assert_eq!(
        run.calls.len(),
        1,
        "only the waiting task gets an event: {:?}",
        run.calls
    );
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

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_skips_and_closes_an_object_on_another_host_or_gone_and_serves_the_rest() {
    let sandbox = Sandbox::new();
    sandbox.answer("pr-acme-app-17", &json!({"state": "open"}));
    sandbox.answer("reviews-acme-app-17", &json!([{"state": "APPROVED"}]));
    sandbox.answer("pr-acme-app-19", &json!({"message": "Not Found"}));
    sandbox.status("pr-acme-app-19", 404);
    let watches = json!([
        {"object": "https://evil.example.com/acme/app/pull/1", "task_id": "task-1",
         "event_types": ["review.approved"], "wait_episode": "ep-2"},
        {"object": "https://github.com/acme/app/pull/17", "task_id": "task-17",
         "event_types": ["review.approved"], "wait_episode": "ep-1"},
        {"object": "https://github.com/acme/app/pull/19", "task_id": "task-19",
         "event_types": ["review.approved"], "wait_episode": "ep-3"},
    ]);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input("github.com", json!({}), json!({}), watches),
    );

    let output = run.output();
    assert_eq!(
        output.closed_watches,
        [
            "https://evil.example.com/acme/app/pull/1",
            "https://github.com/acme/app/pull/17",
            "https://github.com/acme/app/pull/19"
        ]
    );
    assert!(run.stderr.contains("evil.example.com"), "{}", run.stderr);
    assert!(run.stderr.contains("pull/19"), "{}", run.stderr);
    assert_eq!(run.calls.len(), 1, "{:?}", run.calls);
    assert_eq!(run.calls[0][2], "task-17");
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn github_fails_the_run_at_a_rate_limit() {
    let sandbox = Sandbox::new();
    sandbox.answer(
        "pr-acme-app-17",
        &json!({"message": "API rate limit exceeded for user ID 1."}),
    );
    sandbox.status("pr-acme-app-17", 403);
    let watches = json!([
        {"object": "https://github.com/acme/app/pull/17", "task_id": "task-17",
         "event_types": ["review.approved"], "wait_episode": "ep-1"},
    ]);

    let run = sandbox.run(
        "github-pr-review.sh",
        &input("github.com", json!({}), json!({}), watches),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_skips_and_closes_a_bad_or_forbidden_object_and_serves_the_rest() {
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &json!({"state": "opened"}));
    sandbox.answer("approvals-group-app-7", &json!({"approved": true}));
    sandbox.answer("reviewers-group-app-7", &json!([]));
    sandbox.answer("mr-group-app-8", &json!({"message": "403 Forbidden"}));
    sandbox.status("mr-group-app-8", 403);
    let good = "https://gitlab.example.com/group/app/-/merge_requests/7";
    let diffs = "https://gitlab.example.com/group/app/-/merge_requests/6/diffs";
    let slash = "https://gitlab.example.com/group/app/-/merge_requests/5/";
    let forbidden = "https://gitlab.example.com/group/app/-/merge_requests/8";
    let entry = |object: &str, task: &str| {
        json!({"object": object, "task_id": task,
               "event_types": ["review.approved"], "wait_episode": "ep-1"})
    };
    let watches = json!([
        entry(diffs, "task-6"),
        entry(good, "task-7"),
        entry(slash, "task-5"),
        entry(forbidden, "task-8"),
    ]);

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &input(
            "gitlab.example.com",
            json!({"host": "gitlab.example.com"}),
            json!({}),
            watches,
        ),
    );

    let output = run.output();
    let mut closed = output.closed_watches.clone();
    closed.sort();
    let mut expected = vec![diffs, good, slash, forbidden];
    expected.sort_unstable();
    assert_eq!(closed, expected);
    assert!(run.stderr.contains("/diffs"), "{}", run.stderr);
    assert_eq!(run.calls.len(), 1, "{:?}", run.calls);
    assert_eq!(run.calls[0][2], "task-7");
    assert_eq!(run.calls[0][4], "review.approved");
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_fails_the_run_when_the_source_fails() {
    let sandbox = Sandbox::new();
    sandbox.answer("mr-group-app-7", &json!({"message": "oops"}));
    sandbox.status("mr-group-app-7", 500);
    let watches = json!([
        {"object": "https://gitlab.example.com/group/app/-/merge_requests/7", "task_id": "task-7",
         "event_types": ["review.approved"], "wait_episode": "ep-1"},
    ]);

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &input(
            "gitlab.example.com",
            json!({"host": "gitlab.example.com"}),
            json!({}),
            watches,
        ),
    );

    assert_ne!(run.code, Some(0));
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(run.calls.is_empty());
}

/// One event per wait: after the review event the entry is closed, and
/// the merge of the same run is not sent.
#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_delivers_the_review_event_once_and_closes_the_entry() {
    let sandbox = Sandbox::new();
    let name = "group-app-mr-7";
    sandbox.answer(&format!("mr-{name}"), &json!({"state": "merged"}));
    sandbox.answer(&format!("approvals-{name}"), &json!({"approved": true}));
    sandbox.answer(
        &format!("reviewers-{name}"),
        &json!([{"state": "approved"}]),
    );
    let object = "https://gitlab.example.com/group/app-mr/-/merge_requests/7";
    let watches = json!([
        {"object": object, "task_id": "task-7",
         "event_types": ["review.approved", "merged"], "wait_episode": "ep-3"},
    ]);

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &input(
            "gitlab.example.com",
            json!({"host": "gitlab.example.com"}),
            Value::Null,
            watches,
        ),
    );

    let output = run.output();
    assert_eq!(output.state, json!({}));
    assert_eq!(output.closed_watches, [object]);
    let types: Vec<&str> = run.calls.iter().map(|call| call[4].as_str()).collect();
    assert_eq!(types, ["review.approved"]);
}

#[test]
#[ignore = "runs the example scripts with a real sh and jq"]
fn gitlab_delivers_a_merge_without_a_review_and_closes_the_entry() {
    let sandbox = Sandbox::new();
    let name = "group-app-7";
    sandbox.answer(&format!("mr-{name}"), &json!({"state": "merged"}));
    sandbox.answer(&format!("approvals-{name}"), &json!({"approved": false}));
    sandbox.answer(&format!("reviewers-{name}"), &json!([]));
    let object = "https://gitlab.example.com/group/app/-/merge_requests/7";
    let watches = json!([
        {"object": object, "task_id": "task-7",
         "event_types": ["review.approved", "merged"], "wait_episode": "ep-3"},
    ]);

    let run = sandbox.run(
        "gitlab-mr-review.sh",
        &input(
            "gitlab.example.com",
            json!({"host": "gitlab.example.com"}),
            json!({}),
            watches,
        ),
    );

    let output = run.output();
    assert_eq!(output.closed_watches, [object]);
    let types: Vec<&str> = run.calls.iter().map(|call| call[4].as_str()).collect();
    assert_eq!(types, ["merged"]);
}
