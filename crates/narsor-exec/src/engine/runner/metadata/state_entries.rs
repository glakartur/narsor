//! How often each state was entered, behind `max_entries`.

use super::*;

pub(crate) fn state_entry_count(
    execution: &WorkflowExecution,
    state_id: &str,
) -> Result<u32, EngineError> {
    let counts: BTreeMap<String, u32> =
        read_engine_record(execution, METADATA_STATE_ENTRIES_KEY)?.unwrap_or_default();
    Ok(counts.get(state_id).copied().unwrap_or_default())
}

/// Counts one more entry into `state_id` (R10), in the record the next save
/// of `execution` writes.
///
/// It checks no limit: the transition the engine takes checks it first, and
/// the operator's manual transition counts without checking it (R10a).
pub(crate) fn count_entry(
    execution: &mut WorkflowExecution,
    state_id: &str,
) -> Result<(), EngineError> {
    let mut counts: BTreeMap<String, u32> =
        read_engine_record(execution, METADATA_STATE_ENTRIES_KEY)?.unwrap_or_default();
    let count = counts.entry(state_id.to_string()).or_default();
    *count = count.saturating_add(1);
    write_engine_record(execution, METADATA_STATE_ENTRIES_KEY, &counts)
}

/// The entry counts of `execution` (R10), as a decision saves them for an
/// undo to restore.
pub(crate) fn state_entries_value(execution: &WorkflowExecution) -> Result<Value, EngineError> {
    let counts: BTreeMap<String, u32> =
        read_engine_record(execution, METADATA_STATE_ENTRIES_KEY)?.unwrap_or_default();
    Ok(json!(counts))
}

/// Puts back the entry counts `saved` by [`state_entries_value`]; a
/// decision saved before the counts were kept (`null`) leaves them as they
/// are.
pub(crate) fn restore_state_entries(
    execution: &mut WorkflowExecution,
    saved: &Value,
) -> Result<(), EngineError> {
    if saved.is_null() {
        return Ok(());
    }
    let counts: BTreeMap<String, u32> = serde_json::from_value(saved.clone()).map_err(|error| {
        EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: format!("the entry counts saved with the decision are unreadable: {error}"),
        }
    })?;
    write_engine_record(execution, METADATA_STATE_ENTRIES_KEY, &counts)
}
