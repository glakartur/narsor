//! The text `narsor task …` prints about a task (`narsor::cli::task_text`):
//! badges and tones, ages and the outcome of an owner command.
//!
//! These were the pure functions of the old remote TUI mode; the commands are
//! their only reader now.

use chrono::{Duration, TimeZone, Utc};

use narsor::api::{ClientError, HarnessSelectionSnapshot};
use narsor::cli::task_text::{
    age, attention_lines, command_outcome, default_harness_line, detail_lines, display_lines,
    list_line, next_waiting, queue_badge, safe_json, Tone,
};
use narsor::controller_protocol::QueueView;
use narsor::domain::display::DisplayLink;
use narsor::domain::ExecutionStatus;
use narsor::orchestration::{AttentionKind, HarnessUnavailable, OwnerCommand, QueueReason};

mod task_app_support;
use task_app_support::{queued, task, titled, waiting_on, with_runner};

fn queue(reason: QueueReason) -> QueueView {
    QueueView {
        message: reason.describe(),
        reason,
        since: Utc::now(),
    }
}

#[test]
fn each_queue_reason_has_its_own_badge_and_tone() {
    let waiting = queue_badge(&queue(QueueReason::WaitingForCapacity {
        position: 2,
        running: 3,
        limit: 3,
    }));
    assert_eq!(
        (waiting.text.as_str(), waiting.tone),
        ("#2 · 3/3", Tone::Normal)
    );
    let offline = queue_badge(&queue(QueueReason::ControllerOffline));
    assert_eq!(offline.tone, Tone::Warning);
    let unknown = queue_badge(&queue(QueueReason::BackendUnknown {
        detail: "gone".into(),
    }));
    assert_eq!(unknown.tone, Tone::Error);
    assert!(unknown.text.contains("gone"));
    let admitted = queue_badge(&queue(QueueReason::Admitted));
    assert_eq!(
        (admitted.text.as_str(), admitted.tone),
        ("starting…", Tone::Good)
    );
}

#[test]
fn ages_are_short_and_never_negative() {
    let now = Utc.with_ymd_and_hms(2026, 10, 3, 12, 0, 0).unwrap();
    assert_eq!(age(now, now - Duration::seconds(5)), "5s");
    assert_eq!(age(now, now - Duration::minutes(3)), "3m");
    assert_eq!(age(now, now - Duration::hours(2)), "2h");
    assert_eq!(age(now, now + Duration::seconds(30)), "0s");
}

#[test]
fn a_full_backend_on_resume_is_backend_full_not_a_task_failure() {
    let outcome = command_outcome(
        OwnerCommand::Resume,
        &Err(ClientError::Refused {
            code: "backend_at_capacity".into(),
            message: "backend 'k8s' has no free capacity (3 of 3 slots in use)".into(),
        }),
    );
    assert_eq!(outcome.tone, Tone::Warning);
    assert!(outcome.text.starts_with("Backend full"), "{}", outcome.text);
    assert!(outcome.text.contains("keeps its state"));

    let other = command_outcome(
        OwnerCommand::Pause,
        &Err(ClientError::Refused {
            code: "illegal_command".into(),
            message: "no".into(),
        }),
    );
    assert_eq!(other.tone, Tone::Error);
}

#[test]
fn runner_text_never_reaches_a_terminal_as_control_sequences() {
    use narsor::cli::task_text::conversation_lines;
    use narsor::controller_protocol::ConversationResponse;
    use narsor::orchestration::ConversationAttemptSummary;

    let hostile = "x\u{1b}]0;pwned\u{7}y\u{202e}z";
    let clean = |text: &str| !text.contains('\u{1b}') && !text.contains('\u{202e}');

    let refused = command_outcome(
        OwnerCommand::Pause,
        &Err(ClientError::Refused {
            code: "no".into(),
            message: hostile.into(),
        }),
    );
    assert!(clean(&refused.text), "{:?}", refused.text);

    let unknown = queue_badge(&queue(QueueReason::BackendUnknown {
        detail: hostile.into(),
    }));
    assert!(clean(&unknown.text), "{:?}", unknown.text);

    let conversation = ConversationResponse::Available {
        attempts: vec![ConversationAttemptSummary {
            attempt_id: hostile.into(),
            state_id: Some(hostile.into()),
            entries: 1,
            closed: false,
            last_entry_at: None,
        }],
        observed_at: Utc::now(),
        source: Default::default(),
        recorded_at: None,
    };
    for line in conversation_lines(Utc::now(), &conversation) {
        assert!(clean(&line.text), "{:?}", line.text);
    }
}

#[test]
fn a_list_row_shows_the_attention_mark_the_status_and_the_title() {
    let calm = titled(task("t-1", "local", "paused", 1), "NAR-1 login");
    let line = list_line(&calm);
    assert!(line.starts_with("t-1"), "{line}");
    assert!(line.contains("#1"), "{line}");
    assert!(line.contains("paused — NAR-1 login"), "{line}");

    let asking = waiting_on(
        task("t-2", "local", "paused", 2),
        AttentionKind::Question,
        "q-1",
        Some("review"),
    );
    let asking = list_line(&asking);
    assert!(asking.contains("#2         ? paused"), "{asking}");
    assert!(!asking.contains('—'), "no title, no dash: {asking}");

    let wrong = waiting_on(
        task("t-3", "local", "paused", 3),
        AttentionKind::FailedStep,
        "f-1",
        None,
    );
    assert!(list_line(&wrong).contains("#3         ! paused"));
}

#[test]
fn a_list_row_never_carries_what_a_terminal_would_act_on() {
    let hostile = titled(
        task("t-1", "local", "paused", 1),
        "x\u{1b}]0;pwned\u{7}y\u{202e}z",
    );
    let line = list_line(&hostile);
    assert!(
        !line.contains('\u{1b}') && !line.contains('\u{202e}'),
        "{line:?}"
    );
}

#[test]
fn the_details_name_the_title_the_links_and_what_the_task_waits_on() {
    let mut subject = titled(
        waiting_on(
            task("t-1", "local", "paused", 1),
            AttentionKind::Approval,
            "pending-7",
            Some("review"),
        ),
        "NAR-1 login",
    );
    subject.display.links = vec![
        DisplayLink {
            label: "Jira".into(),
            text: "NAR-1".into(),
            url: Some("https://jira.example/NAR-1".into()),
        },
        DisplayLink {
            label: "PR".into(),
            text: String::new(),
            url: None,
        },
    ];
    let text: Vec<String> = detail_lines(&subject, None, None)
        .into_iter()
        .map(|line| line.text)
        .collect();
    let has = |needle: &str| text.iter().any(|line| line.contains(needle));
    assert!(has("Title      NAR-1 login"), "{text:#?}");
    assert!(
        has("Link       Jira: NAR-1 <https://jira.example/NAR-1>"),
        "{text:#?}"
    );
    assert!(has("Link       PR: —"), "{text:#?}");
    assert!(
        has("Waiting    ? approval in 'review' for 0s (id pending-7)"),
        "{text:#?}"
    );
    assert_eq!(display_lines(&task("t-2", "local", "paused", 1)).len(), 0);
    assert!(attention_lines(&task("t-2", "local", "paused", 1)).is_empty());
}

#[test]
fn a_link_that_is_not_http_is_not_printed() {
    let mut subject = task("t-1", "local", "paused", 1);
    subject.display.links = vec![DisplayLink {
        label: "Evil".into(),
        text: "click".into(),
        url: Some("javascript:alert(1)".into()),
    }];
    let lines = display_lines(&subject);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "Link       Evil: click");
}

#[test]
fn the_next_waiting_task_follows_the_list_and_wraps_round() {
    let tasks = vec![
        waiting_on(queued("a", 1), AttentionKind::Question, "q", None),
        queued("b", 2),
        waiting_on(queued("c", 3), AttentionKind::Problem, "p", None),
    ];
    assert_eq!(next_waiting(&tasks, "a").unwrap().task_id.as_str(), "c");
    assert_eq!(next_waiting(&tasks, "b").unwrap().task_id.as_str(), "c");
    assert_eq!(next_waiting(&tasks, "c").unwrap().task_id.as_str(), "a");
    let alone = vec![waiting_on(
        queued("a", 1),
        AttentionKind::Question,
        "q",
        None,
    )];
    assert!(next_waiting(&alone, "a").is_none());
}

#[test]
fn json_output_is_valid_and_free_of_invisible_characters() {
    let hostile = titled(task("t-1", "local", "paused", 1), "a\u{202e}b\u{1b}c");
    let text = safe_json(&vec![hostile]).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed[0]["task_id"], "t-1");
    assert_eq!(parsed[0]["display"]["title"], "abc");
    assert!(!text.contains('\u{202e}'));
}

fn selection(override_harness: Option<&str>) -> HarnessSelectionSnapshot {
    HarnessSelectionSnapshot {
        supported: vec!["claude-code".into(), "codex".into(), "opencode".into()],
        configured: "claude-code".into(),
        override_harness: override_harness.map(str::to_string),
        effective: override_harness.unwrap_or("claude-code").to_string(),
        revision: 0,
    }
}

#[test]
fn the_default_harness_line_says_where_the_harness_comes_from() {
    assert_eq!(
        default_harness_line(&selection(None)),
        "default harness: claude-code (configured)"
    );
    assert_eq!(
        default_harness_line(&selection(Some("opencode"))),
        "default harness: opencode (override; configured claude-code)"
    );
}

#[test]
fn the_default_harness_line_drops_invisible_characters() {
    let line = default_harness_line(&selection(Some("open\u{1b}[2Jcode\u{202e}")));
    assert!(
        !line.contains('\u{1b}') && !line.contains('\u{202e}'),
        "{line:?}"
    );
}

#[test]
fn the_details_say_when_the_task_cannot_follow_the_default_harness() {
    let mut subject = with_runner(
        task("t-1", "k8s", "running", 1),
        ExecutionStatus::Running,
        "build",
    );
    let lines = |subject: &narsor::controller_protocol::TaskSummary| {
        detail_lines(subject, None, None)
            .into_iter()
            .filter(|line| line.text.starts_with("Harness "))
            .collect::<Vec<_>>()
    };
    assert!(lines(&subject).is_empty(), "nothing while it follows");

    subject.runner.as_mut().unwrap().default_harness_unavailable = Some(HarnessUnavailable {
        harness: "open\u{1b}[2Jcode".into(),
        kept: "claude-code".into(),
        reason: "backend 'k8s' runs only\u{202e} claude-code".into(),
    });
    let shown = lines(&subject);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(
        shown[0].text,
        "Harness    open[2Jcode unavailable here, keeping claude-code: \
         backend 'k8s' runs only claude-code"
    );
    assert_eq!(shown[0].tone, Tone::Warning);
}
