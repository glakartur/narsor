//! Static validation of a workflow definition (`WORKFLOW_SCHEMA.md` §5).
//!
//! The validator implements the four rule groups of the contract — graph
//! consistency, action correctness, decision correctness and variable
//! references — and it never stops at the first problem: the author of a
//! workflow should see the whole list at once, so every entry point returns
//! all violations that were found.
//!
//! Reachability is checked in both directions. A forward BFS from
//! `initial_state` finds states nobody can enter; a backward BFS from the
//! terminal states finds states from which the execution could never finish.
//! Cycles are explicitly allowed: the `fix_code -> test -> fix_code` loop of
//! §4 is legal because `test` also has an edge to `create_pr`, so the rule is
//! "a path to some terminal state exists", not "there are no cycles".
//!
//! Template references are only checked where they can be checked statically.
//! `{{ inputs.x }}` must name a declared input; `{{ context.* }}` and anything
//! with filters or other expression syntax is passed through untouched and
//! resolved at runtime (U4).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use crate::domain::{
    ActionConfig, DecisionConfig, OutputLabel, State, StateId, ValidationError, WorkflowDefinition,
    WorkspaceSpec,
};

use super::loader::{compile, LoadError};
use super::schema::WorkflowFile;

/// Validates a compiled workflow definition.
///
/// Returns every violation found, in a deterministic order (states are visited
/// by sorted id), so that the same definition always produces the same report.
pub fn validate(definition: &WorkflowDefinition) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    validate_graph(definition, &mut errors);
    for state_id in sorted_state_ids(definition) {
        let state = &definition.states[state_id];
        validate_action(state, &mut errors);
        validate_decision(state, &mut errors);
        validate_state_templates(definition, state, &mut errors);
    }
    validate_workflow_templates(definition, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validates a workflow file, YAML layer included.
///
/// [`validate`] cannot see every rule of §5 on its own: compilation turns
/// `terminal_states` into a flag on each named state, so an entry naming a
/// state that does not exist simply disappears. This entry point checks the
/// file first and then validates the compiled definition.
///
/// The returned vector is empty when the definition is valid; `LoadError` is
/// reserved for a file that cannot be compiled at all.
pub fn validate_file(file: WorkflowFile) -> Result<Vec<ValidationError>, LoadError> {
    let mut errors = Vec::new();
    {
        let declared: HashSet<&str> = file.states.iter().map(|(id, _)| id.as_str()).collect();
        for terminal in file.terminal_states.ids() {
            if !declared.contains(terminal) {
                errors.push(ValidationError::UnknownTerminalState(terminal.to_string()));
            }
        }
    }

    let definition = compile(file)?;
    if let Err(mut definition_errors) = validate(&definition) {
        errors.append(&mut definition_errors);
    }

    Ok(errors)
}

/// Parses and validates a workflow definition from YAML source.
pub fn validate_source(yaml: &str) -> Result<Vec<ValidationError>, LoadError> {
    let file: WorkflowFile = serde_yaml::from_str(yaml).map_err(LoadError::ParseInline)?;
    validate_file(file)
}

/// Parses and validates a workflow definition from a YAML file on disk.
pub fn validate_path<P: AsRef<Path>>(path: P) -> Result<Vec<ValidationError>, LoadError> {
    let path = path.as_ref();
    let contents = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let file: WorkflowFile =
        serde_yaml::from_str(&contents).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    validate_file(file)
}

// ---------------------------------------------------------------------------
// Group 1: graph consistency
// ---------------------------------------------------------------------------

fn validate_graph(definition: &WorkflowDefinition, errors: &mut Vec<ValidationError>) {
    let initial_exists = definition.states.contains_key(&definition.initial_state);
    if !initial_exists {
        errors.push(ValidationError::InitialStateNotFound(
            definition.initial_state.clone(),
        ));
    }

    for state_id in sorted_state_ids(definition) {
        let state = &definition.states[state_id];

        for label in sorted_transition_labels(state) {
            let target = &state.transitions[label];
            if !definition.states.contains_key(target) {
                errors.push(ValidationError::TransitionToUnknownState(
                    state_id.clone(),
                    label.clone(),
                    target.clone(),
                ));
            }
            if !state.outputs.iter().any(|output| &output.label == label) {
                errors.push(ValidationError::TransitionWithoutOutput {
                    state: state_id.clone(),
                    label: label.clone(),
                });
            }
        }

        // Checked on the compiled list, so a label declared once bare and once
        // as an object is caught the same as a plain repeat.
        let mut declared = HashSet::new();
        let mut reported = HashSet::new();
        for output in &state.outputs {
            let label = output.label.as_str();
            if !declared.insert(label) && reported.insert(label) {
                errors.push(ValidationError::DuplicateOutput {
                    state: state_id.clone(),
                    label: output.label.clone(),
                });
            }
        }

        if state.terminal {
            if !state.transitions.is_empty() {
                errors.push(ValidationError::TerminalStateWithTransitions(
                    state_id.clone(),
                ));
            }
            continue;
        }

        // Every declared output needs somewhere to go — unless the state is
        // terminal, which the contract exempts explicitly (§5.1).
        for output in &state.outputs {
            if !state.transitions.contains_key(&output.label) {
                errors.push(ValidationError::OutputWithoutTransition {
                    state: state_id.clone(),
                    label: output.label.clone(),
                });
            }
        }

        if state.action.is_none() {
            errors.push(ValidationError::NonTerminalStateWithoutAction(
                state_id.clone(),
            ));
        }
    }

    let terminal_states: Vec<&StateId> = sorted_state_ids(definition)
        .into_iter()
        .filter(|id| definition.states[*id].terminal)
        .collect();

    if terminal_states.is_empty() {
        errors.push(ValidationError::NoTerminalState);
    }

    if !initial_exists {
        // Without an entry point, reachability would flag every state and
        // drown the one error that matters.
        return;
    }

    let reachable = reachable_from_initial(definition);
    for state_id in sorted_state_ids(definition) {
        if !reachable.contains(state_id) {
            errors.push(ValidationError::UnreachableState(state_id.clone()));
        }
    }

    if terminal_states.is_empty() {
        return;
    }

    let can_finish = states_that_reach_a_terminal(definition, &terminal_states);
    for state_id in sorted_state_ids(definition) {
        if reachable.contains(state_id) && !can_finish.contains(state_id) {
            errors.push(ValidationError::NoPathToTerminal(state_id.clone()));
        }
    }
}

/// Forward BFS over `transitions`, starting at `initial_state`.
fn reachable_from_initial(definition: &WorkflowDefinition) -> HashSet<&StateId> {
    let mut seen: HashSet<&StateId> = HashSet::new();
    let mut queue: VecDeque<&StateId> = VecDeque::new();

    if let Some((id, _)) = definition.states.get_key_value(&definition.initial_state) {
        seen.insert(id);
        queue.push_back(id);
    }

    while let Some(current) = queue.pop_front() {
        for target in definition.states[current].transitions.values() {
            if let Some((id, _)) = definition.states.get_key_value(target) {
                if seen.insert(id) {
                    queue.push_back(id);
                }
            }
        }
    }

    seen
}

/// Backward BFS from the terminal states: the set of states from which at
/// least one terminal state is reachable. A cycle is only a deadlock when none
/// of its states has an edge leading out of it into this set.
fn states_that_reach_a_terminal<'a>(
    definition: &'a WorkflowDefinition,
    terminal_states: &[&'a StateId],
) -> HashSet<&'a StateId> {
    let mut incoming: HashMap<&StateId, Vec<&StateId>> = HashMap::new();
    for (source, state) in &definition.states {
        for target in state.transitions.values() {
            if let Some((target_id, _)) = definition.states.get_key_value(target) {
                incoming.entry(target_id).or_default().push(source);
            }
        }
    }

    let mut seen: HashSet<&StateId> = HashSet::new();
    let mut queue: VecDeque<&StateId> = VecDeque::new();
    for terminal in terminal_states {
        if seen.insert(terminal) {
            queue.push_back(terminal);
        }
    }

    while let Some(current) = queue.pop_front() {
        if let Some(sources) = incoming.get(current) {
            for source in sources {
                if seen.insert(source) {
                    queue.push_back(source);
                }
            }
        }
    }

    seen
}

// ---------------------------------------------------------------------------
// Group 2: action correctness
// ---------------------------------------------------------------------------

fn validate_action(state: &State, errors: &mut Vec<ValidationError>) {
    let Some(action) = &state.action else {
        return;
    };

    // Field names are the YAML ones, because that is the file the author is
    // looking at while reading the message.
    match action {
        ActionConfig::Agent(agent) => {
            // An *absent* harness is fine: every setting has a value built
            // into the binary, so "no harness anywhere" cannot happen, and
            // that is what keeps an exported workflow portable (D-F).
            //
            // A harness that is present but blank is still a mistake, and
            // still one this can catch: `Some("")` is not `None`, so it would
            // otherwise reach the registry at run time — after the workspace
            // has been cloned and earlier states have run — instead of failing
            // here, for free, the way it used to.
            if agent
                .harness
                .as_deref()
                .is_some_and(|harness| harness.trim().is_empty())
            {
                errors.push(missing_field(state, "agent", "harness"));
            }
            if agent.prompt.trim().is_empty() {
                errors.push(missing_field(state, "agent", "prompt"));
            }
            if state.outputs.is_empty() {
                errors.push(ValidationError::EmptyOutputs {
                    state: state.id.clone(),
                    action_type: "agent",
                });
            }
        }
        ActionConfig::Script(script) => {
            if script.command.trim().is_empty() {
                errors.push(missing_field(state, "script", "command"));
            }
            if state.outputs.is_empty() {
                errors.push(ValidationError::EmptyOutputs {
                    state: state.id.clone(),
                    action_type: "script",
                });
            }
        }
        ActionConfig::Human(human) => {
            if human.instruction.trim().is_empty() {
                errors.push(missing_field(state, "human", "message"));
            }
        }
        ActionConfig::Wait(wait) => {
            if wait.event.trim().is_empty() {
                errors.push(missing_field(state, "wait", "event_type"));
            }
        }
    }
}

fn missing_field(state: &State, action_type: &'static str, field: &'static str) -> ValidationError {
    ValidationError::MissingActionField {
        state: state.id.clone(),
        action_type,
        field,
    }
}

// ---------------------------------------------------------------------------
// Group 3: decision correctness
// ---------------------------------------------------------------------------

fn validate_decision(state: &State, errors: &mut Vec<ValidationError>) {
    // `rules` and `human` need no parameters; an agent decision is a second
    // agent call and needs the same two fields an agent action does.
    if let Some(DecisionConfig::Agent(agent)) = &state.decision {
        if is_blank(agent.harness.as_deref()) {
            errors.push(ValidationError::AgentDecisionMissingField {
                state: state.id.clone(),
                field: "harness",
            });
        }
        if is_blank(agent.prompt.as_deref()) {
            errors.push(ValidationError::AgentDecisionMissingField {
                state: state.id.clone(),
                field: "prompt",
            });
        }
    }
}

fn is_blank(value: Option<&str>) -> bool {
    value.map(str::trim).unwrap_or("").is_empty()
}

// ---------------------------------------------------------------------------
// Group 4: variable references
// ---------------------------------------------------------------------------

fn validate_state_templates(
    definition: &WorkflowDefinition,
    state: &State,
    errors: &mut Vec<ValidationError>,
) {
    let mut templated: Vec<(String, &str)> = Vec::new();

    match &state.action {
        Some(ActionConfig::Agent(agent)) => {
            // The same shape the decision branch below already has.
            if let Some(harness) = &agent.harness {
                templated.push(("action.harness".to_string(), harness.as_str()));
            }
            templated.push(("action.prompt".to_string(), agent.prompt.as_str()));
            if let Some(model) = &agent.model {
                templated.push(("action.model".to_string(), model.as_str()));
            }
        }
        Some(ActionConfig::Script(script)) => {
            templated.push(("action.command".to_string(), script.command.as_str()));
            for (index, arg) in script.args.iter().enumerate() {
                templated.push((format!("action.args[{index}]"), arg.as_str()));
            }
            if let Some(working_dir) = &script.working_dir {
                templated.push(("action.working_dir".to_string(), working_dir.as_str()));
            }
            for (key, value) in &script.env {
                templated.push((format!("action.env.{key}"), value.as_str()));
            }
        }
        Some(ActionConfig::Human(human)) => {
            templated.push(("action.message".to_string(), human.instruction.as_str()));
        }
        Some(ActionConfig::Wait(wait)) => {
            templated.push(("action.event_type".to_string(), wait.event.as_str()));
        }
        None => {}
    }

    match &state.decision {
        Some(DecisionConfig::Agent(agent)) => {
            if let Some(harness) = &agent.harness {
                templated.push(("decision.harness".to_string(), harness.as_str()));
            }
            if let Some(prompt) = &agent.prompt {
                templated.push(("decision.prompt".to_string(), prompt.as_str()));
            }
        }
        Some(DecisionConfig::Human(human)) => {
            if let Some(message) = &human.message {
                templated.push(("decision.message".to_string(), message.as_str()));
            }
        }
        Some(DecisionConfig::Rules) | None => {}
    }

    // The env map has no defined order, so the report is sorted by field.
    templated.sort_by(|left, right| left.0.cmp(&right.0));

    for (field, text) in templated {
        for variable in extract_input_references(text) {
            if !declares_input(definition, &variable) {
                errors.push(ValidationError::UnknownInputReference {
                    state: state.id.clone(),
                    field: field.clone(),
                    variable,
                });
            }
        }
    }
}

fn validate_workflow_templates(definition: &WorkflowDefinition, errors: &mut Vec<ValidationError>) {
    let Some(workspace) = &definition.workspace else {
        return;
    };

    let (field, text) = match workspace {
        WorkspaceSpec::Git { repository, .. } => {
            ("globals.workspace.repository", repository.as_str())
        }
        WorkspaceSpec::Local { path, .. } => ("globals.workspace.path", path.as_str()),
    };

    for variable in extract_input_references(text) {
        if !declares_input(definition, &variable) {
            errors.push(ValidationError::UnknownInputReferenceInGlobals { field, variable });
        }
    }
}

fn declares_input(definition: &WorkflowDefinition, name: &str) -> bool {
    definition.inputs.iter().any(|input| input.name == name)
}

/// Collects the plain `{{ inputs.x }}` references of a string.
///
/// Anything else inside the braces — `context.*`, a filter such as
/// `{{ context.a | default(inputs.b) }}`, or syntax this extractor does not
/// understand — is deliberately skipped: only a runtime context can resolve it,
/// and a validator that guessed here would reject valid workflows.
fn extract_input_references(text: &str) -> Vec<String> {
    let mut references = Vec::new();
    let mut rest = text;

    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            break;
        };
        if let Some(name) = plain_input_reference(after_open[..close].trim()) {
            references.push(name);
        }
        rest = &after_open[close + 2..];
    }

    references
}

/// Returns the input name of a bare `inputs.<name>` expression, or `None` for
/// anything more complicated.
fn plain_input_reference(expression: &str) -> Option<String> {
    let name = expression.strip_prefix("inputs.")?;
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// Ordering helpers
// ---------------------------------------------------------------------------

fn sorted_state_ids(definition: &WorkflowDefinition) -> Vec<&StateId> {
    let mut ids: Vec<&StateId> = definition.states.keys().collect();
    ids.sort();
    ids
}

fn sorted_transition_labels(state: &State) -> Vec<&OutputLabel> {
    let mut labels: Vec<&OutputLabel> = state.transitions.keys().collect();
    labels.sort();
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_plain_input_references() {
        assert_eq!(
            extract_input_references("Build {{ inputs.feature }} now"),
            vec!["feature".to_string()]
        );
    }

    #[test]
    fn passes_through_context_and_filter_expressions() {
        assert!(extract_input_references("{{ context.clarification_text }}").is_empty());
        assert!(extract_input_references(
            "{{ context.clarification_text | default(inputs.feature_description) }}"
        )
        .is_empty());
    }

    #[test]
    fn ignores_an_unterminated_expression() {
        assert!(extract_input_references("{{ inputs.feature").is_empty());
    }
}
