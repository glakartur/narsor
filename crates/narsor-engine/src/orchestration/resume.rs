//! Counting workload losses for the automatic resume (R5, R6).
//!
//! The control plane counts a loss when the task enters `Interrupted`, from
//! where and when the workload was actually lost (remembered already when
//! the task went to `Unknown` first):
//!
//! | loss                                              | count     | next attempt          |
//! |---------------------------------------------------|-----------|-----------------------|
//! | from `PauseRequested`                             | unchanged | none (owner decides)  |
//! | reported under a newer lease epoch than the launch | unchanged | now (at most `limit` in a row, then counted) |
//! | from `Provisioning`, or ready shorter than the window | +1     | delay of that attempt, or exhausted past the limit |
//! | ready at least the stability window               | 1         | now                   |
//!
//! A loss after a run of at least the stability window starts both counts
//! anew first, so a restart loss after it is free again and the next
//! counted loss is attempt 1.
//!
//! The resume policy then only reads the outcome: an attempt that is due
//! and not exhausted ([`TaskRecord::auto_resume_due`]), resumed through
//! `ControlPlane::auto_resume`.

use super::model::{
    Observation, ResumeExhaustion, RunnerState, TaskEvent, TaskEventKind, TaskRecord, WorkloadLoss,
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

/// The prefix of the idempotency keys of the slot-release policy's commands
/// (`slot-release:<generation>`), reserved like [`AUTO_RESUME_KEY_PREFIX`].
pub const SLOT_RELEASE_KEY_PREFIX: &str = "slot-release:";

/// The idempotency key of the policy's pause of the task running at
/// `generation`: one per run, however often a trigger sees it due.
pub fn slot_release_key(generation: u64) -> String {
    format!("{SLOT_RELEASE_KEY_PREFIX}{generation}")
}

/// The prefix of the idempotency keys of the resume that work sent to a
/// paused task causes (`work-resume:<key of the work>`), reserved like
/// [`AUTO_RESUME_KEY_PREFIX`]: a retry of the work resumes the task once.
pub const WORK_RESUME_KEY_PREFIX: &str = "work-resume:";

/// The idempotency key prefixes only a policy may use, each with what it is
/// reserved for: an owner's command may not carry one.
pub const RESERVED_KEY_PREFIXES: [(&str, &str); 3] = [
    (AUTO_RESUME_KEY_PREFIX, "the resume policy"),
    (SLOT_RELEASE_KEY_PREFIX, "the slot-release policy"),
    (
        WORK_RESUME_KEY_PREFIX,
        "the resume that work sent to a paused task causes",
    ),
];

/// The idempotency key of the resume caused by the work with `work_key`.
pub fn work_resume_key(work_key: &str) -> String {
    format!("{WORK_RESUME_KEY_PREFIX}{work_key}")
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
    let started_successfully = loss.from != RunnerState::Provisioning
        && record
            .running_since
            .is_some_and(|since| loss.at - since >= policy.stability_window);
    let restarted = record
        .workload_epoch
        .is_some_and(|epoch| loss.lease_epoch > epoch);
    let resume = &mut record.resume;
    resume.next_attempt_at = None;
    resume.last_loss_uncounted = false;
    // Lost before a safe point: what the owner wanted was a pause.
    if loss.from == RunnerState::PauseRequested {
        return;
    }
    // A runner that stayed up starts the count anew, whatever lost it.
    if started_successfully {
        resume.consecutive_losses = 0;
        resume.uncounted_losses = 0;
    }
    // Found by a controller that took over since the launch (a restart):
    // the loss says nothing about whether the task can run. Only so many in
    // a row, though: past the limit a controller that keeps restarting
    // counts like any other loss, so a crash loop ends in an exhaustion.
    if restarted && resume.uncounted_losses < policy.limit() {
        resume.uncounted_losses += 1;
        resume.last_loss_uncounted = true;
        resume.next_attempt_at = Some(now);
        return;
    }
    resume.consecutive_losses = resume.consecutive_losses.saturating_add(1);
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

/// Pure display helpers over the public task protocol. Re-export these from
/// orchestration for internal callers while clients depend only on protocol.
pub use narsor_protocol::display::{
    exhaustion_loss, loss_state, LossState, EXHAUSTION_REASON_PREFIX, RESUME_ATTEMPT_LIMIT,
    UNSPECIFIED_LOSS,
};

/// Why the policy gave up, for people.
fn exhaustion_reason(loss_reason: Option<&str>) -> String {
    format!(
        "{EXHAUSTION_REASON_PREFIX}{}",
        loss_reason.unwrap_or(UNSPECIFIED_LOSS)
    )
}

/// The loss of a task that went `Interrupted` or `Unknown` before losses
/// were remembered, read from its event log: the runner_state before the last
/// move into either. A log that does not show it reads as a loss while
/// running, which the policy resumes.
pub(crate) fn loss_from_history(record: &TaskRecord, events: &[TaskEvent]) -> WorkloadLoss {
    let lost = |runner_state: RunnerState| {
        matches!(
            runner_state,
            RunnerState::Interrupted | RunnerState::Unknown
        )
    };
    let entered = events
        .windows(2)
        .rev()
        .find(|pair| lost(pair[1].runner_state) && !lost(pair[0].runner_state));
    let reason = record
        .observed
        .as_ref()
        .filter(|observed| matches!(observed.observation, Observation::WorkloadLost { .. }))
        .and_then(|observed| observed.summary.detail.clone());
    match entered {
        Some(pair) => WorkloadLoss {
            from: pair[0].runner_state,
            at: pair[1].at,
            lease_epoch: match pair[1].kind {
                TaskEventKind::ControllerReported { lease_epoch, .. } => lease_epoch,
                _ => record.last_report_epoch.unwrap_or(0),
            },
            reason,
        },
        None => WorkloadLoss {
            from: RunnerState::Running,
            at: record.last_event.at,
            lease_epoch: record.last_report_epoch.unwrap_or(0),
            reason,
        },
    }
}
