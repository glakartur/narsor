//! The replies and streams of steps that later templates read.

use super::*;

/// `reply`, unless it is missing or only whitespace.
pub(crate) fn non_blank(reply: Option<&str>) -> Option<String> {
    reply
        .filter(|reply| !reply.trim().is_empty())
        .map(ToString::to_string)
}

pub(crate) fn step_reply_attempt<'a>(
    execution: &'a WorkflowExecution,
    state_id: &str,
) -> Option<&'a str> {
    execution
        .metadata
        .get(METADATA_ENGINE_KEY)?
        .get(METADATA_STEP_REPLIES_KEY)?
        .get(state_id)?
        .as_str()
}

/// The templates of an action that may use the result of an earlier step: a
/// prompt or message with the references of its parts' `when`.
fn step_templates(action: &ActionConfig) -> Vec<String> {
    match action {
        ActionConfig::Agent(config) => config.prompt.template_pieces(),
        ActionConfig::Profile(config) => config.prompt.template_pieces(),
        ActionConfig::Human(config) => vec![config.instruction.clone()],
        ActionConfig::Decision(config) => config.message.template_pieces(),
        ActionConfig::Script(_) | ActionConfig::Wait(_) => Vec::new(),
    }
}

pub(crate) fn action_step_reply_sources(action: &ActionConfig) -> Vec<String> {
    let mut sources = Vec::new();
    for template in step_templates(action) {
        for source in step_reply_references(&template) {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    sources
}

pub(crate) fn action_step_stream_sources(action: &ActionConfig) -> Vec<(String, String)> {
    let mut sources = Vec::new();
    for template in step_templates(action) {
        for source in step_stream_references(&template) {
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    sources
}

pub(crate) fn workflow_step_reply_sources(definition: &WorkflowDefinition) -> HashSet<String> {
    definition
        .states
        .values()
        .flat_map(|state| state.action.as_ref().into_iter())
        .flat_map(action_step_reply_sources)
        .collect()
}

pub(crate) fn workflow_step_stream_sources(definition: &WorkflowDefinition) -> HashSet<String> {
    definition
        .states
        .values()
        .flat_map(|state| state.action.as_ref().into_iter())
        .flat_map(action_step_stream_sources)
        .map(|(source, _)| source)
        .collect()
}
