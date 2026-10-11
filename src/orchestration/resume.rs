//! Counting workload losses for the automatic resume (R5, R6).
//!
//! The control plane counts a loss when the task enters `Interrupted`, from
//! where and when the workload was actually lost (remembered already when
//! the task went to `Unknown` first):
//!
//! | loss                                              | count     | next attempt          |
//! |---------------------------------------------------|-----------|-----------------------|
//! | from `PauseRequested`                             | unchanged | none (owner decides)  |
//! | reported under a newer lease epoch than the launch | unchanged | now                   |
//! | from `Provisioning`, or ready shorter than the window | +1     | delay of that attempt, or exhausted past the limit |
//! | ready at least the stability window               | 1         | now                   |
//!
//! The resume policy then only reads the outcome: an attempt that is due
//! and not exhausted ([`TaskRecord::auto_resume_due`]), resumed through
//! `ControlPlane::auto_resume`.

use super::model::{
    Observation, ResumeExhaustion, TaskEvent, TaskEventKind, TaskLifecycle, TaskRecord,
    WorkloadLoss,
};
use chrono::{DateTime, Duration, Utc};

/// The prefix of the idempotency keys of the resume policy's commands
/// (`auto-resume:<generation>`), reserved: an owner's key may not use it.
pub const AUTO_RESUME_KEY_PREFIX: &str = "auto-resume:";

/// The idempotency key of the policy's resume of a task interrupted at
/// `generation`: one per interruption, however often the pass sees it.
pub fn auto_resume_key(generation: u64) -> String {
    format!("{AUTO_RESUME_KEY_PREFIX}{generation}")
}

/// How the control plane resumes a task after its workload was lost.
///
/// Fixed for every backend (no per-task or per-workflow setting); only
/// tests shorten it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePolicy {
    /// The delay before each consecutive attempt, first attempt first. Its
    /// length is the limit: one more consecutive loss exhausts it.
    pub attempt_delays: Vec<Duration>,
    /// How long a runner must have been ready for its loss to count as
    /// following a successful start, which starts the count anew.
    pub stability_window: Duration,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        Self {
            attempt_delays: vec![
                Duration::zero(),
                Duration::seconds(30),
                Duration::minutes(2),
                Duration::minutes(10),
                Duration::minutes(30),
            ],
            stability_window: Duration::minutes(10),
        }
    }
}

impl ResumePolicy {
    /// How many consecutive attempts are made at most.
    pub fn limit(&self) -> u32 {
        u32::try_from(self.attempt_delays.len()).unwrap_or(u32::MAX)
    }

    /// The delay before attempt `attempt` (1-based); `None` past the limit.
    fn delay(&self, attempt: u32) -> Option<Duration> {
        let index = usize::try_from(attempt.checked_sub(1)?).ok()?;
        self.attempt_delays.get(index).copied()
    }
}

/// Counts the task's remembered loss as it enters `Interrupted`: plans the
/// next attempt, or gives up with a reason naming the loss.
pub(crate) fn count_loss(record: &mut TaskRecord, policy: &ResumePolicy, now: DateTime<Utc>) {
    let Some(loss) = record.resume.last_loss.clone() else {
        return;
    };
    let resume = &mut record.resume;
    resume.next_attempt_at = None;
    // Lost before a safe point: what the owner wanted was a pause.
    if loss.from == TaskLifecycle::PauseRequested {
        return;
    }
    // Found by a controller that took over since the launch (a restart):
    // the loss says nothing about whether the task can run.
    if record
        .workload_epoch
        .is_some_and(|epoch| loss.lease_epoch > epoch)
    {
        resume.next_attempt_at = Some(now);
        return;
    }
    let started_successfully = loss.from != TaskLifecycle::Provisioning
        && record
            .running_since
            .is_some_and(|since| loss.at - since >= policy.stability_window);
    resume.consecutive_losses = if started_successfully {
        1
    } else {
        resume.consecutive_losses.saturating_add(1)
    };
    match policy.delay(resume.consecutive_losses) {
        Some(delay) => resume.next_attempt_at = Some(now + delay),
        None => {
            resume.exhausted = Some(ResumeExhaustion {
                reason: exhaustion_reason(loss.reason.as_deref()),
                at: now,
                announced: false,
            })
        }
    }
}

/// What every [`ResumeExhaustion::reason`] begins with; a client that words
/// it in another language strips it and keeps the loss after it.
pub const EXHAUSTION_REASON_PREFIX: &str = "could not resume automatically: ";

/// The loss in [`ResumeExhaustion::reason`] when the controller said nothing
/// about it.
pub const UNSPECIFIED_LOSS: &str = "the workload was lost";

/// Why the policy gave up, for people.
fn exhaustion_reason(loss_reason: Option<&str>) -> String {
    format!(
        "{EXHAUSTION_REASON_PREFIX}{}",
        loss_reason.unwrap_or(UNSPECIFIED_LOSS)
    )
}

/// The loss of a task that went `Interrupted` or `Unknown` before losses
/// were remembered, read from its event log: the lifecycle before the last
/// move into either. A log that does not show it reads as a loss while
/// running, which the policy resumes.
pub(crate) fn loss_from_history(record: &TaskRecord, events: &[TaskEvent]) -> WorkloadLoss {
    let lost = |lifecycle: TaskLifecycle| {
        matches!(
            lifecycle,
            TaskLifecycle::Interrupted | TaskLifecycle::Unknown
        )
    };
    let entered = events
        .windows(2)
        .rev()
        .find(|pair| lost(pair[1].lifecycle) && !lost(pair[0].lifecycle));
    let reason = record
        .observed
        .as_ref()
        .filter(|observed| matches!(observed.observation, Observation::WorkloadLost { .. }))
        .and_then(|observed| observed.summary.detail.clone());
    match entered {
        Some(pair) => WorkloadLoss {
            from: pair[0].lifecycle,
            at: pair[1].at,
            lease_epoch: match pair[1].kind {
                TaskEventKind::ControllerReported { lease_epoch, .. } => lease_epoch,
                _ => record.last_report_epoch.unwrap_or(0),
            },
            reason,
        },
        None => WorkloadLoss {
            from: TaskLifecycle::Running,
            at: record.last_event.at,
            lease_epoch: record.last_report_epoch.unwrap_or(0),
            reason,
        },
    }
}
