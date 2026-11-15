//! Reading and writing the engine's records inside `metadata.engine`.

use super::*;

/// Reads one record of the engine's scratch area.
pub(crate) fn read_engine_record<T: serde::de::DeserializeOwned>(
    execution: &WorkflowExecution,
    key: &str,
) -> Result<Option<T>, EngineError> {
    let Some(value) = execution
        .metadata
        .get(METADATA_ENGINE_KEY)
        .and_then(|engine| engine.get(key))
    else {
        return Ok(None);
    };
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|error| EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: error.to_string(),
        })
}

/// Writes one record of the engine's scratch area.
pub(crate) fn write_engine_record<T: Serialize>(
    execution: &mut WorkflowExecution,
    key: &str,
    record: &T,
) -> Result<(), EngineError> {
    let value =
        serde_json::to_value(record).map_err(|error| EngineError::CorruptEngineMetadata {
            execution_id: execution.id.clone(),
            message: error.to_string(),
        })?;
    engine_metadata_mut(execution).insert(key.to_string(), value);
    Ok(())
}

/// Drops one key of the engine's scratch area, if it is there.
pub(crate) fn clear_engine_metadata(execution: &mut WorkflowExecution, key: &str) {
    if let Some(engine) = execution
        .metadata
        .get_mut(METADATA_ENGINE_KEY)
        .and_then(Value::as_object_mut)
    {
        engine.remove(key);
    }
}

/// Returns the engine-owned metadata object, normalizing records written
/// before `metadata` or its engine subobject were guaranteed to be objects.
pub(crate) fn engine_metadata_mut(
    execution: &mut WorkflowExecution,
) -> &mut serde_json::Map<String, Value> {
    if !execution.metadata.is_object() {
        execution.metadata = json!({});
    }
    let metadata = execution
        .metadata
        .as_object_mut()
        .expect("metadata was just made an object");
    let engine = metadata
        .entry(METADATA_ENGINE_KEY.to_string())
        .or_insert_with(|| json!({}));
    if !engine.is_object() {
        *engine = json!({});
    }
    engine
        .as_object_mut()
        .expect("engine metadata was just made an object")
}
