//! Transition rules of the orchestration state machine.
//!
//! Pure functions, kept apart from persistence so the whole diagram can be
//! read in one place:
//!
//! ```text
//! Queued ──controller accepts──▶ Provisioning ──runner ready──▶ Running
//! Running ──owner pause──▶ PauseRequested ──safe point + compute removed──▶ Stopped
//! Stopped ──owner resume──▶ Provisioning
//! Stopped ──owner resume, backend full──▶ Queued (front, no reservation)
//! Provisioning | Running | PauseRequested ──workload lost──▶ Interrupted | Unknown
//! Unknown ──loss confirmed (workload dead, storage present)──▶ Interrupted
//! Interrupted ──resume policy──▶ Provisioning                     (automatic)
//! Interrupted | Unknown ──owner start/resume──▶ Provisioning
//! any non-archival ──owner archive──▶ Archiving ──deletion acknowledged──▶ Archived
//! ```
//!
//! The automatic resume is an ordinary `Resume` the control plane's resume
//! policy issues (with [`super::model::CommandActor::Policy`]); this module
//! does not tell it from the owner's. *Whether* the policy resumes lives in
//! the control plane: a task lost while pausing, `Stopped` itself, and a task
//! whose attempts are exhausted still wait for their owner, and `Unknown`
//! is never resumed before its loss is confirmed.
//!
//! Archive wins: once a task is archival, no command or observation leads out
//! of it again, and a repeated archive converges on the tombstone.

use super::model::{Observation, OwnerCommand, RunnerState};

/// What an owner command does to a runner_state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandEffect {
    /// Move to this state and bump the desired generation.
    Transition(RunnerState),
    /// Archive repeated on an archival task: record it, change nothing else.
    ArchiveConverged,
    /// Not allowed from the current state.
    Illegal,
}

/// The effect of `command` on a task in `current`.
pub(crate) fn command_effect(current: RunnerState, command: OwnerCommand) -> CommandEffect {
    use RunnerState::*;
    match (command, current) {
        (OwnerCommand::Archive, Archiving | Archived) => CommandEffect::ArchiveConverged,
        (OwnerCommand::Archive, _) => CommandEffect::Transition(Archiving),
        (OwnerCommand::Pause, Running) => CommandEffect::Transition(PauseRequested),
        (OwnerCommand::Resume, Stopped | Interrupted | Unknown) => {
            CommandEffect::Transition(Provisioning)
        }
        (OwnerCommand::Start, Interrupted | Unknown) => CommandEffect::Transition(Provisioning),
        _ => CommandEffect::Illegal,
    }
}

/// What a controller observation does to a runner_state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationEffect {
    /// Move to this state; the desired generation stays.
    Transition(RunnerState),
    /// Valid, but the runner_state is already where the observation leads (a
    /// redelivery) or the observation carries no runner_state meaning.
    Unchanged,
    /// Contradicts the current state.
    Illegal,
}

/// The effect of `observation` on a task in `current`.
pub(crate) fn observation_effect(
    current: RunnerState,
    observation: Observation,
) -> ObservationEffect {
    use RunnerState::*;
    match (observation, current) {
        (Observation::Status, _) => ObservationEffect::Unchanged,

        (Observation::StartAccepted, Queued) => ObservationEffect::Transition(Provisioning),
        (Observation::StartAccepted, Provisioning) => ObservationEffect::Unchanged,

        (Observation::RunnerReady, Provisioning) => ObservationEffect::Transition(Running),
        (Observation::RunnerReady, Running) => ObservationEffect::Unchanged,

        (Observation::PausedAtSafePoint, PauseRequested) => ObservationEffect::Transition(Stopped),
        (Observation::PausedAtSafePoint, Stopped) => ObservationEffect::Unchanged,

        // Never a replacement on the controller's word alone: whether the
        // task is resumed is the control plane's resume policy's decision.
        (Observation::WorkloadLost { state_known }, Provisioning | Running | PauseRequested) => {
            ObservationEffect::Transition(if state_known { Interrupted } else { Unknown })
        }
        (Observation::WorkloadLost { .. }, Interrupted | Unknown) => ObservationEffect::Unchanged,
        // Archive already won; losing the workload only helps it along.
        (Observation::WorkloadLost { .. }, Archiving) => ObservationEffect::Unchanged,

        // Only an uncertain loss has anything to confirm; everywhere else
        // the confirmation is late or moot and changes nothing.
        (Observation::LossConfirmed, Unknown) => ObservationEffect::Transition(Interrupted),
        (Observation::LossConfirmed, _) => ObservationEffect::Unchanged,

        (Observation::ResourcesDeleted, Archiving) => ObservationEffect::Transition(Archived),
        (Observation::ResourcesDeleted, Archived) => ObservationEffect::Unchanged,

        _ => ObservationEffect::Illegal,
    }
}
