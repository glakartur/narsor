//! The text `narsor task …` prints about a task (`narsor::cli::task_text`):
//! badges and tones, ages and the outcome of an owner command.
//!
//! These were the pure functions of the old remote TUI mode; the commands are
//! their only reader now.

use chrono::{Duration, TimeZone, Utc};

use narsor::api::ClientError;
use narsor::cli::task_text::{age, command_outcome, queue_badge, Tone};
use narsor::controller_protocol::QueueView;
use narsor::orchestration::{OwnerCommand, QueueReason};

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
