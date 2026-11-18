//! The text `narsor task workflows|validate|capabilities|repo` print
//! (`narsor::cli::control_text`), the text of `narsor-cli usage`, the text
//! of `narsor-cli observer queue …` (`narsor::cli::observer_text`) and the
//! typing of `task create --file` inputs.

use chrono::{DateTime, TimeZone, Utc};
use narsor::cli::control_text::{
    capabilities_lines, repository_row, typed_inputs, usage_lines, validation_lines,
    workflow_lines, NO_USAGE,
};
use narsor::cli::observer_text::{queue_list_output, queue_show_output};
use narsor_engine::controller_protocol::{
    ValidateWorkflowResponse, WorkflowInputView, WorkflowSource,
};
use narsor_engine::domain::ParamType;
use narsor_protocol::controller::{
    HarnessUsageResponse, HarnessUsageView, ObserverQueueListResponse, ObserverQueueSummary,
    ObserverQueueView, ObserverView, QueueEntryErrorView, QueueEntryObserverView, QueueEntryView,
    UsageLimit, UsageStatus,
};
use serde_json::json;

use crate::task_app_support;
use task_app_support::{capabilities, catalogue_workflow, repository, workflow_input};

fn input(name: &str, kind: ParamType, required: bool) -> WorkflowInputView {
    workflow_input(name, kind, required, None)
}

fn texts(lines: Vec<narsor::cli::task_text::ToneLine>) -> Vec<String> {
    lines.into_iter().map(|line| line.text).collect()
}

#[test]
fn inputs_are_typed_by_what_the_definition_declares() {
    let mut greeting = input("greeting", ParamType::String, true);
    greeting.default = None;
    let mut flag = input("flag", ParamType::Boolean, false);
    flag.default = Some(json!(false));
    let declared = vec![greeting, input("count", ParamType::Integer, false), flag];
    let values = typed_inputs(&declared, &["greeting=42".into(), "count=7".into()]).unwrap();
    assert_eq!(values["greeting"], json!("42"), "a string stays text");
    assert_eq!(values["count"], json!(7));
    assert_eq!(
        values["flag"],
        json!(false),
        "the default fills what is left out"
    );
}

#[test]
fn a_text_input_passes_a_multi_line_value_unchanged() {
    let declared = vec![input("prompt", ParamType::Text, true)];
    let values = typed_inputs(&declared, &["prompt=first\nsecond".into()]).unwrap();
    assert_eq!(values["prompt"], json!("first\nsecond"));
}

#[test]
fn a_wrongly_typed_unknown_or_missing_input_is_a_usage_error() {
    let declared = vec![
        input("greeting", ParamType::String, true),
        input("count", ParamType::Integer, true),
        input("flag", ParamType::Boolean, false),
    ];
    let wrong = typed_inputs(&declared, &["count=many".into()]).unwrap_err();
    assert!(wrong.message.contains("not a whole number"), "{wrong:?}");
    let flag = typed_inputs(&declared, &["flag=maybe".into()]).unwrap_err();
    assert!(flag.message.contains("boolean"), "{flag:?}");
    let unknown = typed_inputs(&declared, &["nope=1".into()]).unwrap_err();
    assert!(
        unknown.message.contains("greeting, count, flag"),
        "{unknown:?}"
    );
    let missing = typed_inputs(&declared, &[]).unwrap_err();
    assert!(
        missing.message.contains("greeting, count"),
        "every missing input at once: {missing:?}"
    );
    let malformed = typed_inputs(&declared, &["greeting".into()]).unwrap_err();
    assert!(malformed.message.contains("name=value"), "{malformed:?}");
}

#[test]
fn a_workflow_of_the_catalogue_shows_source_description_and_inputs() {
    let mut workflow = catalogue_workflow(
        "implement-feature",
        "Builds a feature.",
        vec![input("ticket", ParamType::String, true)],
    );
    workflow.overrides = true;
    let text = texts(workflow_lines(&workflow));
    assert_eq!(
        text[0],
        "implement-feature v1  [built-in]  (hides another copy of the same name)"
    );
    assert_eq!(text[1], "    Builds a feature.");
    assert_eq!(text[2], "    input ticket: String required");

    let mut broken = catalogue_workflow("broken", "x", Vec::new());
    broken.valid = false;
    broken.source = WorkflowSource::User;
    broken.violations = vec!["state 'a' leads nowhere".into()];
    let text = texts(workflow_lines(&broken));
    assert!(text[0].ends_with("[user]  INVALID"), "{text:?}");
    assert!(text.iter().any(|line| line.contains("leads nowhere")));
}

#[test]
fn a_verdict_lists_every_violation_or_the_definition() {
    let invalid = ValidateWorkflowResponse {
        valid: false,
        name: None,
        version: None,
        state_count: 0,
        initial_state: None,
        inputs: Vec::new(),
        workspaces: Vec::new(),
        violations: vec!["first".into(), "second".into()],
    };
    assert_eq!(
        texts(validation_lines(&invalid)),
        ["Invalid — 2 violation(s):", "  - first", "  - second"]
    );
    let valid = ValidateWorkflowResponse {
        valid: true,
        name: Some("greet".into()),
        version: Some(2),
        state_count: 2,
        initial_state: Some("start".into()),
        inputs: vec![input("who", ParamType::String, true)],
        workspaces: Vec::new(),
        violations: Vec::new(),
    };
    let text = texts(validation_lines(&valid));
    assert_eq!(text[0], "Valid: greet v2");
    assert_eq!(text[1], "    input who: String required");
}

#[test]
fn capabilities_say_what_is_refused_and_why() {
    let text = texts(capabilities_lines(&capabilities(
        false,
        Some("not a local daemon"),
    )));
    assert_eq!(text[0], "Connection:             staging");
    assert_eq!(text[1], "Workflow paths:         no (not a local daemon)");
    let local = texts(capabilities_lines(&capabilities(true, None)));
    assert_eq!(local[1], "Workflow paths:         yes");
    assert_eq!(local[2], "Local repository paths: yes");
}

#[test]
fn a_repository_row_matches_the_local_listing() {
    let row = repository_row(&repository("api", "https://example.com/api.git"));
    assert!(row.starts_with("api "), "{row}");
    assert!(row.contains("github") && row.contains("main"), "{row}");
    assert!(row.ends_with("https://example.com/api.git"), "{row}");
}

#[test]
fn what_a_server_sends_never_reaches_a_terminal_as_control_sequences() {
    let hostile = "x\u{1b}]0;pwned\u{7}y\u{202e}z";
    let clean = |text: &str| !text.contains('\u{1b}') && !text.contains('\u{202e}');
    let mut workflow = catalogue_workflow(
        hostile,
        hostile,
        vec![input(hostile, ParamType::String, true)],
    );
    workflow.violations = vec![hostile.into()];
    for line in workflow_lines(&workflow) {
        assert!(clean(&line.text), "{:?}", line.text);
    }
    assert!(clean(&repository_row(&repository(hostile, hostile))));
}

// ---------------------------------------------------------------------------
// `narsor-cli usage` (U9)
// ---------------------------------------------------------------------------

fn usage_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 31, 14, 2, 0).unwrap()
}

fn limit(
    window: &str,
    utilization: f64,
    resets_at: DateTime<Utc>,
    status: Option<UsageStatus>,
) -> UsageLimit {
    UsageLimit {
        window: window.to_string(),
        utilization,
        resets_at,
        status,
    }
}

fn harness_usage(harness: &str, limits: Vec<UsageLimit>) -> HarnessUsageView {
    HarnessUsageView {
        harness: harness.to_string(),
        observed_at: usage_now(),
        received_at: usage_now(),
        limits,
    }
}

fn claude_usage() -> HarnessUsageView {
    harness_usage(
        "claude-code",
        vec![
            limit(
                "5h",
                0.01,
                Utc.with_ymd_and_hms(2026, 10, 31, 18, 12, 0).unwrap(),
                Some(UsageStatus::Allowed),
            ),
            limit(
                "7d",
                0.9,
                Utc.with_ymd_and_hms(2026, 11, 2, 17, 2, 0).unwrap(),
                Some(UsageStatus::AllowedWarning),
            ),
        ],
    )
}

fn usage_texts(response: &HarnessUsageResponse) -> Vec<String> {
    texts(usage_lines(response, usage_now(), &Utc))
}

#[test]
fn usage_prints_a_block_per_harness_with_aligned_limit_rows() {
    let response = HarnessUsageResponse {
        harnesses: vec![claude_usage()],
    };
    assert_eq!(
        usage_texts(&response),
        vec![
            "Claude Code (claude-code), reported 2026-10-31 14:02",
            "  5h    1%  allowed   resets 2026-10-31 18:12  (in 4h10m)",
            "  7d   90%  warning   resets 2026-11-02 17:02  (in 2d3h)",
        ]
    );
}

#[test]
fn usage_blocks_are_kept_apart_by_an_empty_line_and_a_missing_status_is_a_dash() {
    let resets = Utc.with_ymd_and_hms(2026, 10, 31, 14, 47, 0).unwrap();
    let response = HarnessUsageResponse {
        harnesses: vec![
            claude_usage(),
            harness_usage(
                "codex",
                vec![
                    limit("90m", 1.0, resets, Some(UsageStatus::Rejected)),
                    limit("7d", 0.3, resets, None),
                ],
            ),
        ],
    };
    let lines = usage_texts(&response);
    assert_eq!(lines[3], "");
    assert_eq!(
        &lines[4..],
        [
            "Codex (codex), reported 2026-10-31 14:02",
            "  90m  100%  rejected  resets 2026-10-31 14:47  (in 45m)",
            "  7d    30%  -         resets 2026-10-31 14:47  (in 45m)",
        ]
    );
}

#[test]
fn usage_without_a_harness_says_there_is_no_data() {
    assert_eq!(
        usage_texts(&HarnessUsageResponse::default()),
        vec![NO_USAGE.to_string()]
    );
    assert_eq!(NO_USAGE, "No usage limit data.");
}

#[test]
fn usage_text_from_the_server_carries_no_control_characters() {
    let mut view = claude_usage();
    view.harness = "claude\u{1b}[2J-code\u{202e}".to_string();
    view.limits[0].window = "5\u{1b}]8;;x\u{7}h".to_string();
    let lines = usage_texts(&HarnessUsageResponse {
        harnesses: vec![view],
    });
    for line in &lines {
        assert!(
            !line.contains('\u{1b}') && !line.contains('\u{202e}') && !line.contains('\u{7}'),
            "{line:?}"
        );
    }
    assert!(lines[0].contains("claude[2J-code"), "{lines:?}");
}

// ---------------------------------------------------------------------------
// `narsor-cli observer queue …` (U10)
// ---------------------------------------------------------------------------

fn queue_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 17, 12, 0, 0).unwrap()
}

/// An `on_episodes` observer `name` of `queue` on a cron schedule, with
/// the script versions given.
fn queue_observer(
    name: &str,
    queue: &str,
    approved: Option<u32>,
    latest: Option<u32>,
) -> ObserverView {
    serde_json::from_value(json!({
        "observer_id": format!("obs-{name}"),
        "name": name,
        "source": "gitlab",
        "state": if approved.is_some() { "active" } else { "draft" },
        "effect": "none",
        "queue": queue,
        "mode": "on_episodes",
        "cron": "*/5 * * * *",
        "task_limit_per_run": 10,
        "approved_version": approved,
        "latest_version": latest,
        "worker_missing": false,
        "consecutive_failures": 0,
        "created_at": queue_now(),
    }))
    .unwrap()
}

fn review_entry(observers: Vec<QueueEntryObserverView>) -> QueueEntryView {
    QueueEntryView {
        entry_id: "entry-1".into(),
        task_id: "task-1".into(),
        step: "review_work".into(),
        episode: "input-3".into(),
        started_at: queue_now() - chrono::Duration::minutes(25),
        params: [
            ("label".to_string(), "w CR".to_string()),
            (
                "mr_url".to_string(),
                "https://gitlab/a/-/merge_requests/17".to_string(),
            ),
        ]
        .into(),
        observers,
    }
}

#[test]
fn queue_list_aligns_a_row_per_queue_and_says_when_there_is_none() {
    let listing = ObserverQueueListResponse {
        queues: vec![
            ObserverQueueSummary {
                queue: "mr-review".into(),
                mode: Some("on_episodes".into()),
                observers: 2,
                entries: 13,
            },
            ObserverQueueSummary {
                queue: "unserved".into(),
                mode: None,
                observers: 0,
                entries: 1,
            },
        ],
    };

    let text = queue_list_output(&listing);

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "QUEUE                    MODE         OBSERVERS ENTRIES",
            "mr-review                on_episodes          2      13",
            "unserved                 -                    0       1",
        ]
    );
    assert!(queue_list_output(&ObserverQueueListResponse::default()).starts_with("No queues."));
}

#[test]
fn queue_show_says_how_each_observer_processed_each_entry() {
    let view = ObserverQueueView {
        queue: "mr-review".into(),
        mode: Some("on_episodes".into()),
        observers: vec![
            queue_observer("gitlab-mr", "mr-review", Some(1), Some(2)),
            queue_observer("gitlab-ci", "mr-review", Some(3), Some(3)),
        ],
        entries: vec![review_entry(vec![
            QueueEntryObserverView {
                observer: "gitlab-ci".into(),
                running: true,
                last_run_at: None,
                last_error: None,
            },
            QueueEntryObserverView {
                observer: "gitlab-mr".into(),
                running: false,
                last_run_at: Some(queue_now() - chrono::Duration::minutes(2)),
                last_error: Some(QueueEntryErrorView {
                    run_id: "obs-gitlab-mr.7".into(),
                    status: "failed".into(),
                    finished_at: queue_now() - chrono::Duration::minutes(2),
                    exit_code: Some(3),
                    error: "the script exited with code 3".into(),
                }),
            },
        ])],
    };

    let text = queue_show_output(&view, queue_now());

    assert!(
        text.starts_with("Queue: mr-review\nMode: on_episodes"),
        "{text}"
    );
    assert!(
        text.contains(
            "    script: version 2 not approved: approve it with `narsor-cli observer approve \
             gitlab-mr --version 2`"
        ),
        "{text}"
    );
    assert_eq!(
        text.matches("not approved").count(),
        1,
        "an approved newest version is not marked: {text}"
    );
    assert!(text.contains("cron '*/5 * * * *' (UTC)"), "{text}");
    assert!(
        text.contains("  task-1                   review_work          input-3              25m\n"),
        "{text}"
    );
    assert!(
        text.contains("    params: label=w CR, mr_url=https://gitlab/a/-/merge_requests/17\n"),
        "{text}"
    );
    assert!(text.contains("    gitlab-ci: running\n"), "{text}");
    assert!(
        text.contains(
            "    gitlab-mr: last run 2m ago; last error: failed 2m ago, exit 3: the script \
             exited with code 3\n"
        ),
        "{text}"
    );
}

#[test]
fn queue_text_from_the_server_carries_no_control_characters() {
    let hostile = "x\u{1b}]0;pwned\u{7}y\u{202e}z";
    let mut entry = review_entry(vec![QueueEntryObserverView {
        observer: hostile.into(),
        running: false,
        last_run_at: None,
        last_error: Some(QueueEntryErrorView {
            run_id: hostile.into(),
            status: hostile.into(),
            finished_at: queue_now(),
            exit_code: None,
            error: hostile.into(),
        }),
    }]);
    entry.task_id = hostile.into();
    entry.step = hostile.into();
    entry.episode = hostile.into();
    entry.params = [(hostile.to_string(), hostile.to_string())].into();
    let view = ObserverQueueView {
        queue: hostile.into(),
        mode: Some(hostile.into()),
        observers: vec![queue_observer(hostile, hostile, None, Some(1))],
        entries: vec![entry],
    };
    let listing = ObserverQueueListResponse {
        queues: vec![ObserverQueueSummary {
            queue: hostile.into(),
            mode: Some(hostile.into()),
            observers: 1,
            entries: 1,
        }],
    };

    for text in [
        queue_show_output(&view, queue_now()),
        queue_list_output(&listing),
    ] {
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}') && !text.contains('\u{202e}'),
            "{text:?}"
        );
    }
}
