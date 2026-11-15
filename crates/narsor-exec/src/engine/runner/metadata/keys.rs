//! Keys of the engine's own records inside `metadata.engine`.

/// Key of the engine's own scratch area inside `WorkflowExecution::metadata`.
pub const METADATA_ENGINE_KEY: &str = "engine";

/// Key of the persisted mid-step state inside the engine's scratch area.
pub const METADATA_PENDING_STEP_KEY: &str = "pending_step";

/// Key of the record of the agent or script step behind the latest
/// transition (KTD8, KTD11).
pub const METADATA_LAST_AGENT_STEP_KEY: &str = "last_agent_step";

/// Key of an agent output that was rejected and still awaits its reprompt
/// (KTD7).
pub const METADATA_OUTPUT_REJECTION_KEY: &str = "output_rejection";

/// Key of a step whose output the conductor is rescuing (F1, U10).
pub const METADATA_OUTPUT_RESCUE_KEY: &str = "output_rescue";

/// A one-shot operator note to attach to the next recovered agent attempt.
pub(crate) const METADATA_OPERATOR_RECOVERY_NOTE_KEY: &str = "operator_recovery_note";

pub(crate) const METADATA_AGENT_INPUT_KEY: &str = "agent_input";

/// Set once the execution recorded that its step profile file could not be
/// read; cleared when a `profile` step writes the file again.
pub(crate) const METADATA_UNREADABLE_STEP_PROFILE_KEY: &str = "unreadable_step_profile_noted";

/// The key of the last comment on a pause the engine was given.
pub(crate) const METADATA_RESUME_MESSAGE_KEY: &str = "resume_message_key";

pub(crate) const METADATA_AGENT_SESSIONS_KEY: &str = "completed_agent_sessions";

pub(crate) const METADATA_STEP_REPLIES_KEY: &str = "step_replies";

pub(crate) const METADATA_STEP_STREAMS_KEY: &str = "step_streams";

pub(crate) const METADATA_STATE_ENTRIES_KEY: &str = "state_entries";
