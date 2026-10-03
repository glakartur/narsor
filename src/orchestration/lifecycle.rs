//! Transition rules of the orchestration state machine.
//!
//! Pure functions, kept apart from persistence so the whole diagram can be
//! read in one place:
//!
//! ```text
//! Queued ──controller accepts──▶ Provisioning ──runner ready──▶ Running
//! Running ──owner pause──▶ PauseRequested ──safe point + compute removed──▶ Paused
//! Paused ──owner resume──▶ Provisioning
//! Provisioning | Running | PauseRequested ──workload lost──▶ Interrupted | Unknown
//! Interrupted | Unknown ──owner start/resume──▶ Provisioning      (never automatic)
//! any non-archival ──owner archive──▶ Archiving ──deletion acknowledged──▶ Archived
//! ```
//!
//! Archive wins: once a task is archival, no command or observation leads out
//! of it again, and a repeated archive converges on the tombstone.

use super::model::{Observation, OwnerCommand, TaskLifecycle};

/// What an owner command does to a lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandEffect {
    /// Move to this state and bump the desired generation.
    Transition(TaskLifecycle),
    /// Archive repeated on an archival task: record it, change nothing else.
    ArchiveConverged,
    /// Not allowed from the current state.
    Illegal,
}

/// The effect of `command` on a task in `current`.
pub(crate) fn command_effect(current: TaskLifecycle, command: OwnerCommand) -> CommandEffect {
    use TaskLifecycle::*;
    match (command, current) {
        (OwnerCommand::Archive, Archiving | Archived) => CommandEffect::ArchiveConverged,
        (OwnerCommand::Archive, _) => CommandEffect::Transition(Archiving),
        (OwnerCommand::Pause, Running) => CommandEffect::Transition(PauseRequested),
        (OwnerCommand::Resume, Paused | Interrupted | Unknown) => {
            CommandEffect::Transition(Provisioning)
        }
        (OwnerCommand::Start, Interrupted | Unknown) => CommandEffect::Transition(Provisioning),
        _ => CommandEffect::Illegal,
    }
}

/// What a controller observation does to a lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationEffect {
    /// Move to this state; the desired generation stays.
    Transition(TaskLifecycle),
    /// Valid, but the lifecycle is already where the observation leads (a
    /// redelivery) or the observation carries no lifecycle meaning.
    Unchanged,
    /// Contradicts the current state.
    Illegal,
}

/// The effect of `observation` on a task in `current`.
pub(crate) fn observation_effect(
    current: TaskLifecycle,
    observation: Observation,
) -> ObservationEffect {
    use TaskLifecycle::*;
    match (observation, current) {
        (Observation::Status, _) => ObservationEffect::Unchanged,

        (Observation::StartAccepted, Queued) => ObservationEffect::Transition(Provisioning),
        (Observation::StartAccepted, Provisioning) => ObservationEffect::Unchanged,

        (Observation::RunnerReady, Provisioning) => ObservationEffect::Transition(Running),
        (Observation::RunnerReady, Running) => ObservationEffect::Unchanged,

        (Observation::PausedAtSafePoint, PauseRequested) => ObservationEffect::Transition(Paused),
        (Observation::PausedAtSafePoint, Paused) => ObservationEffect::Unchanged,

        // Never a replacement: the task waits for an explicit owner command.
        (Observation::WorkloadLost { state_known }, Provisioning | Running | PauseRequested) => {
            ObservationEffect::Transition(if state_known { Interrupted } else { Unknown })
        }
        (Observation::WorkloadLost { .. }, Interrupted | Unknown) => ObservationEffect::Unchanged,
        // Archive already won; losing the workload only helps it along.
        (Observation::WorkloadLost { .. }, Archiving) => ObservationEffect::Unchanged,

        (Observation::ResourcesDeleted, Archiving) => ObservationEffect::Transition(Archived),
        (Observation::ResourcesDeleted, Archived) => ObservationEffect::Unchanged,

        _ => ObservationEffect::Illegal,
    }
}
