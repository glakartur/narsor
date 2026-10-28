//! The text `narsor task …` prints about a task (`narsor::cli::task_text`):
//! badges and tones, ages and the outcome of an owner command.
//!
//! These were the pure functions of the old remote TUI mode; the commands are
//! their only reader now.

use chrono::{Duration, TimeZone, Utc};

use narsor::api::{ClientError, HarnessSelectionSnapshot};
use narsor::cli::task_text::{
    activity_age, age, attention_lines, command_outcome, conversation_lines, conversation_origin,
    default_harness_line, detail_lines, display_lines, event_line, list_line, next_waiting,
    queue_badge, record_unreadable_detail, safe_json, state_hint, Tone,
};
use narsor_engine::controller_protocol::{
    ConversationResponse, ConversationUnavailableReason, ExecutionEventsSource, QueueView,
    CONVERSATION_RECORD_UNREADABLE, CONVERSATION_RECORD_UNREADABLE_PREFIX,
};
use narsor_engine::domain::display::DisplayLink;
use narsor_engine::domain::{ExecutionStatus, TaskId};
use narsor_engine::orchestration::{
    AttentionKind, ConversationAttemptSummary, HarnessUnavailable, OwnerCommand, QueueReason,
    RunnerState, TaskEvent, TaskEventKind, EXHAUSTION_REASON_PREFIX, UNSPECIFIED_LOSS,
};

mod task_app_support;
use task_app_support::{at, exhausted, interrupted, queued, task, titled, waiting_on, with_runner};

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
fn activity_ages_use_two_units_when_helpful_and_never_go_negative() {
    assert_eq!(activity_age(0), "<1m");
    assert_eq!(activity_age(59), "<1m");
    assert_eq!(activity_age(12 * 60), "12m");
    assert_eq!(activity_age(130 * 60), "2h 10m");
    assert_eq!(activity_age(24 * 60 * 60), "1d");
    assert_eq!(activity_age(3 * 24 * 60 * 60), "3d");
    assert_eq!(activity_age(-10), "<1m");
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
    use narsor_engine::controller_protocol::ConversationResponse;
    use narsor_engine::orchestration::ConversationAttemptSummary;

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
    assert!(line.contains("paused · no slot — NAR-1 login"), "{line}");

    let asking = waiting_on(
        task("t-2", "local", "paused", 2),
        AttentionKind::Question,
        "q-1",
        Some("review"),
    );
    let asking = list_line(&asking);
    assert!(asking.contains("#2         ? paused · no slot"), "{asking}");
    assert!(!asking.contains('—'), "no title, no dash: {asking}");

    let wrong = waiting_on(
        task("t-3", "local", "paused", 3),
        AttentionKind::FailedStep,
        "f-1",
        None,
    );
    assert!(list_line(&wrong).contains("#3         ! paused · no slot"));
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
        supported: vec![
            "claude-code".into(),
            "codex".into(),
            "github-copilot".into(),
            "opencode".into(),
        ],
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
    let lines = |subject: &narsor_engine::controller_protocol::TaskSummary| {
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

fn attempt(id: &str, entries: u64) -> ConversationAttemptSummary {
    ConversationAttemptSummary {
        attempt_id: id.to_string(),
        state_id: Some("build".to_string()),
        entries,
        closed: true,
        last_entry_at: None,
    }
}

fn stored(
    attempts: Vec<ConversationAttemptSummary>,
    recorded_at: Option<chrono::DateTime<Utc>>,
) -> ConversationResponse {
    ConversationResponse::Available {
        attempts,
        observed_at: at(),
        source: ExecutionEventsSource::Storage,
        recorded_at,
    }
}

#[test]
fn a_conversation_from_storage_says_how_old_its_record_is() {
    let lines = conversation_lines(
        at(),
        &stored(vec![attempt("a-1", 3)], Some(at() - Duration::minutes(5))),
    );
    assert_eq!(
        lines[0].text,
        "Conversation from record, 5 min old — 1 attempt(s) (c: read)"
    );
    assert_eq!(lines[0].tone, Tone::Normal);
    assert_eq!(lines.len(), 2, "the attempt follows: {lines:?}");
}

#[test]
fn a_record_age_reads_like_the_tui_buckets() {
    let origin = |seconds: i64| {
        conversation_origin(
            at(),
            ExecutionEventsSource::Storage,
            Some(at() - Duration::seconds(seconds)),
        )
    };
    assert_eq!(origin(10), "Conversation from record, written just now");
    assert_eq!(origin(-30), "Conversation from record, written just now");
    assert_eq!(origin(59 * 60), "Conversation from record, 59 min old");
    assert_eq!(origin(3 * 3600 + 5), "Conversation from record, 3 h old");
    assert_eq!(origin(2 * 86_400), "Conversation from record, 2 d old");
    assert_eq!(
        conversation_origin(at(), ExecutionEventsSource::Storage, None),
        "Conversation from record"
    );
    assert_eq!(
        conversation_origin(at(), ExecutionEventsSource::Runner, Some(at())),
        "Conversation live"
    );
}

#[test]
fn a_conversation_from_the_runner_is_live() {
    let live = ConversationResponse::Available {
        attempts: vec![attempt("a-1", 3)],
        observed_at: at() - Duration::seconds(4),
        source: ExecutionEventsSource::Runner,
        recorded_at: None,
    };
    let lines = conversation_lines(at(), &live);
    assert_eq!(
        lines[0].text,
        "Conversation live — 1 attempt(s), reported 4s ago (c: read)"
    );
    assert_eq!(lines[0].tone, Tone::Good);
}

#[test]
fn a_conversation_without_a_source_from_an_older_server_is_live() {
    let older: ConversationResponse = serde_json::from_value(serde_json::json!({
        "status": "available",
        "attempts": [],
        "observed_at": at(),
    }))
    .unwrap();
    let lines = conversation_lines(at(), &older);
    assert!(lines[0].text.starts_with("Conversation live"), "{lines:?}");
}

#[test]
fn an_empty_record_says_nothing_was_recorded() {
    let lines = conversation_lines(at(), &stored(Vec::new(), None));
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].text,
        "Conversation from record — nothing was recorded"
    );
    assert_eq!(lines[0].tone, Tone::Muted);
}

#[test]
fn an_unreadable_record_shows_what_went_wrong_once() {
    let unreadable = ConversationResponse::Unavailable {
        reason: ConversationUnavailableReason::StorageUnreadable,
        last_attempts: Vec::new(),
        observed_at: None,
        message: Some(format!(
            "{CONVERSATION_RECORD_UNREADABLE_PREFIX}permission denied\u{1b}[2J"
        )),
    };
    let lines = conversation_lines(at(), &unreadable);
    assert_eq!(
        lines[0].text,
        "Conversation unavailable: its record could not be read: permission denied[2J"
    );
    assert_eq!(lines[0].tone, Tone::Warning);

    let plain = ConversationResponse::Unavailable {
        reason: ConversationUnavailableReason::Paused,
        last_attempts: Vec::new(),
        observed_at: None,
        message: None,
    };
    assert_eq!(
        conversation_lines(at(), &plain)[0].text,
        "Conversation unavailable: the task is paused; its compute is gone"
    );
}

#[test]
fn a_refused_page_of_an_unreadable_record_gives_its_detail() {
    let refused = ClientError::Refused {
        code: CONVERSATION_RECORD_UNREADABLE.to_string(),
        message: format!("{CONVERSATION_RECORD_UNREADABLE_PREFIX}disk on fire"),
    };
    assert_eq!(
        record_unreadable_detail(&refused).as_deref(),
        Some("disk on fire")
    );
    let other = ClientError::Refused {
        code: "conversation_unavailable".to_string(),
        message: "no runner".to_string(),
    };
    assert_eq!(record_unreadable_detail(&other), None);
}

fn resume_event(kind: TaskEventKind) -> TaskEvent {
    TaskEvent {
        sequence: 7,
        task_id: TaskId::parse("t-1").unwrap(),
        at: at(),
        runner_state: RunnerState::Interrupted,
        generation: 2,
        kind,
    }
}

#[test]
fn an_automatic_resume_event_names_the_attempt_and_the_limit() {
    let line = event_line(&resume_event(TaskEventKind::AutoResumed {
        attempt: 2,
        limit: 5,
        loss_reason: None,
    }));
    assert!(
        line.ends_with("resumed automatically after runner loss (attempt 2/5)"),
        "{line}"
    );
    assert!(line.contains("interrupted"), "{line}");
}

#[test]
fn an_automatic_resume_after_an_uncounted_restart_names_no_attempt() {
    let line = event_line(&resume_event(TaskEventKind::AutoResumed {
        attempt: 0,
        limit: 5,
        loss_reason: None,
    }));
    assert!(
        line.ends_with(
            "resumed automatically after a controller restart lost the runner \
             (not counted toward the 5 attempts)"
        ),
        "{line}"
    );
    assert!(!line.contains("attempt 0"), "{line}");
}

#[test]
fn an_exhaustion_event_names_the_attempts_and_the_loss_once() {
    let line = event_line(&resume_event(TaskEventKind::AutoResumeExhausted {
        attempts: 5,
        reason: format!("{EXHAUSTION_REASON_PREFIX}pod evicted\u{1b}[2J"),
    }));
    assert!(
        line.ends_with("automatic resume gave up after 5 attempts: pod evicted[2J"),
        "{line}"
    );
    assert!(!line.contains(EXHAUSTION_REASON_PREFIX), "{line}");

    let single = event_line(&resume_event(TaskEventKind::AutoResumeExhausted {
        attempts: 1,
        reason: format!("{EXHAUSTION_REASON_PREFIX}{UNSPECIFIED_LOSS}"),
    }));
    assert!(
        single.ends_with(&format!(
            "automatic resume gave up after 1 attempt: {UNSPECIFIED_LOSS}"
        )),
        "{single}"
    );
}

#[test]
fn show_on_an_interrupted_task_tells_what_it_waits_for() {
    let hint = |subject| state_hint(&subject).unwrap();

    let planned = hint(interrupted("t-1", RunnerState::Running, 2, true));
    assert!(
        planned.text.contains("resumed automatically (attempt 2/5)"),
        "{planned:?}"
    );
    assert_eq!(planned.tone, Tone::Warning);

    let gave_up = hint(exhausted(
        interrupted("t-1", RunnerState::Running, 5, false),
        &format!("{EXHAUSTION_REASON_PREFIX}pod evicted"),
    ));
    assert!(
        gave_up
            .text
            .starts_with("could not resume automatically: pod evicted; it still holds"),
        "{gave_up:?}"
    );
    assert_eq!(gave_up.tone, Tone::Error);

    let pausing = hint(interrupted("t-1", RunnerState::PauseRequested, 1, false));
    assert!(
        pausing
            .text
            .starts_with("the workload was lost while pausing"),
        "{pausing:?}"
    );
    assert_eq!(pausing.tone, Tone::Error);

    let older = hint(task("t-1", "local", "interrupted", 1));
    assert!(
        older
            .text
            .starts_with("the workload was lost; it still holds"),
        "{older:?}"
    );
    for line in [&planned, &gave_up, &pausing, &older] {
        assert!(!line.text.contains("on its own"), "{line:?}");
    }

    let detail = detail_lines(
        &interrupted("t-1", RunnerState::Running, 3, true),
        None,
        None,
    );
    assert!(
        detail
            .iter()
            .any(|line| line.text.contains("(attempt 3/5)")),
        "{detail:?}"
    );
}

#[test]
fn the_task_text_shows_the_state_and_whether_a_slot_is_held() {
    use narsor_engine::orchestration::TaskState;
    use task_app_support::without_slot;

    let finished = without_slot(titled(
        with_runner(
            task("t-1", "local", "running", 1),
            ExecutionStatus::Completed,
            "done",
        ),
        "NAR-1",
    ));
    assert_eq!(finished.state, TaskState::Finished);
    let line = list_line(&finished);
    assert!(line.contains("done · no slot — NAR-1"), "{line}");
    let detail = detail_lines(&finished, None, None);
    assert!(
        detail
            .iter()
            .any(|line| line.text.starts_with("State      done · no slot")),
        "{detail:?}"
    );

    let running = with_runner(
        task("t-2", "local", "running", 2),
        ExecutionStatus::Running,
        "edit",
    );
    assert!(list_line(&running).contains("running"), "running");
    assert!(!list_line(&running).contains("no slot"));
    let detail = detail_lines(&running, None, None);
    assert!(
        detail
            .iter()
            .any(|line| line.text.starts_with("State      running · slot held")),
        "{detail:?}"
    );

    let asking = without_slot(task_app_support::with_pending(
        task("t-3", "local", "running", 3),
        serde_json::json!({
            "execution_id": "exec-1",
            "state_id": "plan",
            "action_execution_id": "act-1",
            "instruction": "Which one?",
            "fields": [{ "name": "answer", "type": "text" }],
            "outputs": [],
            "episode": "ep-1",
            "requested_at": "2026-10-09T11:54:00Z"
        }),
    ));
    assert_eq!(asking.state, TaskState::HumanNeeded);
    let hint = state_hint(&asking).unwrap();
    assert!(hint.text.contains("holds no slot"), "{hint:?}");
    assert!(list_line(&asking).contains("awaiting input · no slot"));
}

#[test]
fn a_summary_without_a_state_reads_as_unknown_and_never_panics() {
    let mut value = serde_json::to_value(task("t-1", "local", "running", 1)).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("state");
    object.remove("holds_slot");
    let old: narsor_engine::controller_protocol::TaskSummary =
        serde_json::from_value(value).unwrap();
    assert!(list_line(&old).contains("unknown"));
    assert!(state_hint(&old).is_none());
    assert!(!detail_lines(&old, None, None).is_empty());
}

#[test]
fn a_resume_that_waits_for_a_slot_says_so() {
    use narsor_engine::orchestration::TaskState;

    let mut resumed = task("t-1", "local", "queued", 1);
    resumed.state = TaskState::Queued;
    let response = |queued_for_slot| narsor_engine::controller_protocol::OwnerCommandResponse {
        receipt: serde_json::from_value(serde_json::json!({
            "idempotency_key": "k",
            "command": "resume",
            "expected_generation": 1,
            "accepted_at": at(),
            "generation_after": 2,
            "runner_state_after": "queued",
            "revision": 5,
            "actor": "owner",
        }))
        .expect("a receipt"),
        replayed: false,
        task: resumed.clone(),
        queued_for_slot,
    };
    let waits = command_outcome(OwnerCommand::Resume, &Ok(response(true)));
    assert!(
        waits.text.contains("waits first in the queue for a slot"),
        "{waits:?}"
    );
    let starts = command_outcome(OwnerCommand::Resume, &Ok(response(false)));
    assert!(!starts.text.contains("waits"), "{starts:?}");
}

#[test]
fn the_backend_limit_prints_the_figures_of_the_control_plane_in_a_fixed_order() {
    use narsor::cli::task_text::backend_limit_lines;
    use narsor_protocol::controller::{BackendLimitSource, BackendLimitView};

    let lines = backend_limit_lines(&BackendLimitView {
        backend_id: "k8s".to_string(),
        effective: 2,
        source: BackendLimitSource::AdHoc,
        file_value: 3,
        override_value: Some(2),
        slots_held: 3,
        overshoot: 1,
        waiting_to_stop: 1,
    });

    assert_eq!(
        lines,
        vec![
            "Backend: k8s",
            "Limit: 2 (ad hoc, until the control plane restarts)",
            "File: 3",
            "Ad hoc: 2",
            "Slots held: 3",
            "Over the limit: 1",
            "Waiting to stop: 1",
        ]
    );
}
