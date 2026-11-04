//! Static validation of a workflow definition (`docs/dev/workflow-schema.md` §5).
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
//! The `display` section (§3.6) is checked with the same rules, apart from
//! its labels, which must be non-empty and unique.
//!
//! Template references are only checked where they can be checked statically.
//! `{{ inputs.x }}` must name a declared input and `{{ artifacts.x }}` an
//! artifact some step defines; `{{ context.* }}` and anything
//! with filters or other expression syntax is passed through untouched and
//! resolved at runtime (U4).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use crate::domain::{
    ActionConfig, DecisionConfig, ImageSpec, OutputLabel, RepositorySource, SchemaVersion, State,
    StateId, ValidationError, WorkflowDefinition, LOCAL_VARIABLE_PREFIX, WORKSPACE_ROOT,
};
use crate::workspace::provisioner::is_usable_directory_name;

use super::loader::{compile, LoadError};
use super::schema::WorkflowFile;
use super::template::{
    artifact_references, check_syntax, step_reply_references, step_stream_references,
    unsupported_step_references, DeclaredNames,
};

/// Validates a compiled workflow definition.
///
/// Returns every violation found, in a deterministic order (states are visited
/// by sorted id), so that the same definition always produces the same report.
pub fn validate(definition: &WorkflowDefinition) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let declared = DeclaredNames::of(definition);

    validate_graph(definition, &mut errors);
    for state_id in sorted_state_ids(definition) {
        let state = &definition.states[state_id];
        validate_action(state, &mut errors);
        validate_choice_behavior(definition, state, &mut errors);
        validate_step_contract(state, &mut errors);
        validate_decision(state, &mut errors);
        validate_state_templates(definition, &declared, state, &mut errors);
        validate_state_image(state, &mut errors);
        validate_step_workspace(definition, state, &mut errors);
    }
    validate_repositories(definition, &mut errors);
    validate_display(definition, &mut errors);
    if let Some(image) = &definition.globals.image {
        validate_image("globals.image", image, &mut errors);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_choice_behavior(
    definition: &WorkflowDefinition,
    state: &State,
    errors: &mut Vec<ValidationError>,
) {
    for choice in &state.choices {
        if choice.requires_human_approval && !matches!(state.action, Some(ActionConfig::Agent(_))) {
            errors.push(ValidationError::InvalidChoiceBehavior {
                state: state.id.clone(),
                label: choice.label.clone(),
                reason: "requires_human_approval is only valid on an agent choice".to_string(),
            });
        }
        let Some(field_name) = choice.resume_comment_field.as_deref() else {
            continue;
        };
        // The field may be optional: the engine demands a comment only when
        // this choice is made (KTD12).
        let valid_field = match &state.action {
            Some(ActionConfig::Human(human)) => human.inputs.iter().any(|field| {
                field.name == field_name && field.field_type == crate::domain::HumanInputType::Text
            }),
            _ => false,
        };
        let valid_target = state.transitions.get(&choice.label).is_some_and(|target| {
            definition.states.get(target).is_some_and(|target_state| {
                matches!(target_state.action, Some(ActionConfig::Agent(_)))
            })
        });
        if !valid_field || !valid_target {
            errors.push(ValidationError::InvalidChoiceBehavior {
                state: state.id.clone(),
                label: choice.label.clone(),
                reason: "resume_comment_field requires a text input and an agent transition target"
                    .to_string(),
            });
        }
    }
}

/// The step contract: what a step defines, and that each choice requires
/// only what its step defines (R9, R11, R21).
fn validate_step_contract(state: &State, errors: &mut Vec<ValidationError>) {
    let defines_anything = !state.variables.is_empty() || !state.artifacts.is_empty();
    match &state.action {
        Some(ActionConfig::Agent(_)) | Some(ActionConfig::Script(_)) => {}
        _ if !defines_anything => {}
        other => errors.push(ValidationError::DefinitionsNotAllowed {
            state: state.id.clone(),
            action_type: other.as_ref().map_or("terminal", action_type_name),
        }),
    }

    let is_agent = matches!(state.action, Some(ActionConfig::Agent(_)));
    for variable in &state.variables {
        let bare_name = variable
            .name
            .strip_prefix(LOCAL_VARIABLE_PREFIX)
            .unwrap_or(&variable.name);
        if !is_plain_name(bare_name) {
            errors.push(invalid_name(state, "variable", &variable.name));
        }
        if variable.is_local() && !is_agent {
            errors.push(ValidationError::LocalVariableOutsideAgent {
                state: state.id.clone(),
                name: variable.name.clone(),
            });
        }
    }
    for artifact in &state.artifacts {
        if !is_plain_name(&artifact.name) {
            errors.push(invalid_name(state, "artifact", &artifact.name));
        }
    }

    for choice in &state.choices {
        for name in &choice.requires_variables {
            if !state
                .variables
                .iter()
                .any(|variable| &variable.name == name)
            {
                errors.push(ValidationError::UndefinedRequiredVariable {
                    state: state.id.clone(),
                    label: choice.label.clone(),
                    name: name.clone(),
                });
            }
        }
        for name in &choice.requires_artifacts {
            if !state
                .artifacts
                .iter()
                .any(|artifact| &artifact.name == name)
            {
                errors.push(ValidationError::UndefinedRequiredArtifact {
                    state: state.id.clone(),
                    label: choice.label.clone(),
                    name: name.clone(),
                });
            }
        }
    }
}

/// Letters, digits and `_`, not starting with a digit.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.as_bytes()[0].is_ascii_digit()
}

fn invalid_name(state: &State, kind: &'static str, name: &str) -> ValidationError {
    ValidationError::InvalidDefinitionName {
        state: state.id.clone(),
        kind,
        name: name.to_string(),
    }
}

fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
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
            if !state.choices.iter().any(|choice| &choice.label == label) {
                errors.push(ValidationError::TransitionWithoutChoice {
                    state: state_id.clone(),
                    label: label.clone(),
                });
            }
        }

        // Checked on the compiled list, so a label declared once bare and once
        // as an object is caught the same as a plain repeat.
        let mut declared = HashSet::new();
        let mut reported = HashSet::new();
        for choice in &state.choices {
            let label = choice.label.as_str();
            if !declared.insert(label) && reported.insert(label) {
                errors.push(ValidationError::DuplicateChoice {
                    state: state_id.clone(),
                    label: choice.label.clone(),
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

        // Every declared choice needs somewhere to go — unless the state is
        // terminal, which the contract exempts explicitly (§5.1).
        for choice in &state.choices {
            if !state.transitions.contains_key(&choice.label) {
                errors.push(ValidationError::ChoiceWithoutTransition {
                    state: state_id.clone(),
                    label: choice.label.clone(),
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
    if let Some(limit) = state.max_entries {
        if limit == 0 || !matches!(state.action.as_ref(), Some(ActionConfig::Agent(_))) {
            errors.push(ValidationError::InvalidMaxEntries {
                state: state.id.clone(),
                value: limit,
            });
        }
    }
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
            if state.choices.is_empty() {
                errors.push(ValidationError::EmptyChoices {
                    state: state.id.clone(),
                    action_type: "agent",
                });
            }
        }
        ActionConfig::Script(script) => {
            if script.command.trim().is_empty() {
                errors.push(missing_field(state, "script", "command"));
            }
            if state.choices.is_empty() {
                errors.push(ValidationError::EmptyChoices {
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
// Environment: image overrides (R15)
// ---------------------------------------------------------------------------

/// A step image must parse and satisfy its digest policy, and only a state
/// that runs something may carry one.
///
/// Checked here rather than when the file is deserialized so that a bad
/// reference is one entry in the full report, beside every other problem,
/// instead of a file that does not load. Whether a `resolve`-policy tag
/// actually resolves is a question for task creation, not for the file.
fn validate_state_image(state: &State, errors: &mut Vec<ValidationError>) {
    let Some(image) = &state.image else {
        return;
    };
    if state.terminal {
        errors.push(ValidationError::ImageOnTerminalState(state.id.clone()));
        return;
    }
    validate_image(&format!("states.{}.image", state.id), image, errors);
}

fn validate_image(field: &str, image: &ImageSpec, errors: &mut Vec<ValidationError>) {
    if let Err(error) = image.check() {
        errors.push(ValidationError::InvalidImage {
            field: field.to_string(),
            reason: error.to_string(),
        });
    }
}

// ---------------------------------------------------------------------------
// Group 4: variable references
// ---------------------------------------------------------------------------

fn validate_state_templates(
    definition: &WorkflowDefinition,
    declared: &DeclaredNames,
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
        let allowed = field == "action.prompt" || field == "action.message";
        for source in step_reply_references(text) {
            if !allowed {
                errors.push(ValidationError::StepReplyNotAllowed {
                    state: state.id.clone(),
                    field: field.clone(),
                });
            } else if !definition.states.get(&source).is_some_and(|source_state| {
                matches!(source_state.action.as_ref(), Some(ActionConfig::Agent(_)))
            }) {
                errors.push(ValidationError::InvalidStepReplySource {
                    state: state.id.clone(),
                    step_id: source,
                    field: field.clone(),
                });
            }
        }
        for (source, _) in step_stream_references(text) {
            if !allowed {
                errors.push(ValidationError::StepStreamNotAllowed {
                    state: state.id.clone(),
                    field: field.clone(),
                });
            } else if !definition.states.get(&source).is_some_and(|source_state| {
                matches!(source_state.action.as_ref(), Some(ActionConfig::Script(_)))
            }) {
                errors.push(ValidationError::InvalidStepStreamSource {
                    state: state.id.clone(),
                    step_id: source,
                    field: field.clone(),
                });
            }
        }
        for reference in unsupported_step_references(text) {
            errors.push(ValidationError::UnsupportedStepReference {
                state: state.id.clone(),
                field: field.clone(),
                reference,
            });
        }
        for variable in extract_input_references(text) {
            if !declares_input(definition, &variable) {
                errors.push(ValidationError::UnknownInputReference {
                    state: state.id.clone(),
                    field: field.clone(),
                    variable,
                });
            }
        }
        for artifact in artifact_references(text) {
            if !declared.declares_artifact(&artifact) {
                errors.push(ValidationError::UnknownArtifactReference {
                    state: state.id.clone(),
                    field: field.clone(),
                    artifact,
                });
            }
        }
    }
}

/// The name of a repository field as the author wrote it: a converted `v1`
/// definition reports `globals.workspace.*`, a `v2` one
/// `globals.repositories[i].*` (R4).
fn repository_field(definition: &WorkflowDefinition, index: usize, field: &str) -> String {
    match definition.source_schema {
        SchemaVersion::V1 => format!("globals.workspace.{field}"),
        SchemaVersion::V2 => format!("globals.repositories[{index}].{field}"),
    }
}

/// The rules of `globals.repositories`: usable and distinct names, exactly one
/// primary repository, a branch for every registered one, and templates that
/// only name declared inputs.
fn validate_repositories(definition: &WorkflowDefinition, errors: &mut Vec<ValidationError>) {
    let invalid =
        |field: String, reason: String| ValidationError::InvalidRepository { field, reason };

    let mut names = HashSet::new();
    for (index, repository) in definition.repositories.iter().enumerate() {
        // A converted `v1` name is generated, never the author's.
        if definition.source_schema == SchemaVersion::V2 {
            let field = repository_field(definition, index, "name");
            if !is_usable_directory_name(&repository.name) || repository.name.contains("{{") {
                errors.push(invalid(
                    field,
                    format!("'{}' is not a usable folder name", repository.name),
                ));
            } else if !names.insert(repository.name.as_str()) {
                errors.push(invalid(
                    field,
                    format!("the name '{}' is used more than once", repository.name),
                ));
            }
            if matches!(repository.source, RepositorySource::Git { .. })
                && repository
                    .default_branch
                    .as_deref()
                    .is_none_or(str::is_empty)
            {
                errors.push(invalid(
                    repository_field(definition, index, "default_branch"),
                    "a registered repository needs a default branch".to_string(),
                ));
            }
        }

        let (field, text) = match &repository.source {
            RepositorySource::Git { repository } => ("repository", repository.as_str()),
            RepositorySource::Local { path } => ("path", path.as_str()),
        };
        let field = repository_field(definition, index, field);
        for variable in extract_input_references(text) {
            if !declares_input(definition, &variable) {
                errors.push(ValidationError::UnknownInputReferenceInGlobals {
                    field: field.clone(),
                    variable,
                });
            }
        }
        if !step_reply_references(text).is_empty() || !step_stream_references(text).is_empty() {
            errors.push(ValidationError::InvalidDisplay {
                field,
                reason: "step values are not supported in workspace fields".to_string(),
            });
        }
    }

    if definition.source_schema == SchemaVersion::V2 && !definition.repositories.is_empty() {
        let primaries = definition
            .repositories
            .iter()
            .filter(|repository| repository.primary)
            .count();
        if primaries != 1 {
            errors.push(invalid(
                "globals.repositories".to_string(),
                format!("exactly one repository must be primary, found {primaries}"),
            ));
        }
    }
}

/// The rules of a step's `workspace` section (KTD7).
fn validate_step_workspace(
    definition: &WorkflowDefinition,
    state: &State,
    errors: &mut Vec<ValidationError>,
) {
    let Some(section) = &state.workspace else {
        return;
    };
    let invalid = |field: &'static str, reason: String| ValidationError::InvalidStepWorkspace {
        state: state.id.clone(),
        field,
        reason,
    };
    let declared = |name: &str| definition.repositories.iter().any(|r| r.name == name);

    if section.write.is_empty() && section.read.is_empty() {
        errors.push(invalid(
            "write",
            "the section must list at least one repository in 'write' or 'read'".to_string(),
        ));
    }
    for (field, names) in [("write", &section.write), ("read", &section.read)] {
        let mut seen = HashSet::new();
        for name in names {
            if !declared(name) {
                errors.push(invalid(field, format!("unknown repository '{name}'")));
            } else if !seen.insert(name.as_str()) {
                errors.push(invalid(field, format!("'{name}' is listed more than once")));
            }
        }
    }
    for name in &section.write {
        if section.read.contains(name) {
            errors.push(invalid(
                "read",
                format!("'{name}' is listed in both 'write' and 'read'"),
            ));
        }
    }
    if let Some(root) = &section.root {
        let listed = section.write.contains(root) || section.read.contains(root);
        if root != WORKSPACE_ROOT && !listed {
            errors.push(invalid(
                "root",
                format!("'{root}' is neither '{WORKSPACE_ROOT}' nor a repository listed in 'write' or 'read'"),
            ));
        }
    }
}

/// The `display` section (§3.6): its templates, and that its labels are
/// usable and distinct.
///
/// `{{ inputs.x }}` must be declared like anywhere else, but a variable
/// without a value is *not* an error here — a display is rendered before
/// the variables it shows exist — so only what rendering could never make
/// sense of (a filter, an unclosed `{{`) is reported besides.
fn validate_display(definition: &WorkflowDefinition, errors: &mut Vec<ValidationError>) {
    let Some(display) = &definition.display else {
        return;
    };

    let mut templated: Vec<(String, &str)> = Vec::new();
    if let Some(title) = &display.title {
        templated.push(("display.title".to_string(), title.as_str()));
    }
    let mut labels = HashSet::new();
    for (index, link) in display.links.iter().enumerate() {
        let field = format!("display.links[{index}]");
        if link.label.trim().is_empty() {
            errors.push(ValidationError::InvalidDisplay {
                field: format!("{field}.label"),
                reason: "the label must not be empty".to_string(),
            });
        } else if !labels.insert(link.label.as_str()) {
            errors.push(ValidationError::InvalidDisplay {
                field: format!("{field}.label"),
                reason: format!("the label '{}' is used more than once", link.label),
            });
        }
        if let Some(text) = &link.text {
            templated.push((format!("{field}.text"), text.as_str()));
        }
        templated.push((format!("{field}.url"), link.url.as_str()));
    }

    for (field, text) in templated {
        if let Err(error) = check_syntax(text, &field) {
            errors.push(ValidationError::InvalidDisplay {
                field: field.clone(),
                reason: error.to_string(),
            });
        }
        if !step_reply_references(text).is_empty() || !step_stream_references(text).is_empty() {
            errors.push(ValidationError::InvalidDisplay {
                field: field.clone(),
                reason: "step values are not supported in display fields".to_string(),
            });
        }
        for variable in extract_input_references(text) {
            if !declares_input(definition, &variable) {
                errors.push(ValidationError::UnknownInputReferenceInDisplay {
                    field: field.clone(),
                    variable,
                });
            }
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
            "{{ context.clarification_text | default(inputs.description) }}"
        )
        .is_empty());
    }

    #[test]
    fn ignores_an_unterminated_expression() {
        assert!(extract_input_references("{{ inputs.feature").is_empty());
    }

    const TAIL: &str = r#"
states:
  work:
    action: { type: script, command: ./a.sh }
    choices: [success]
    transitions: { success: done }
  done:
    description: end
terminal_states: [done]
"#;

    fn errors_of(header: &str, step_workspace: &str) -> Vec<ValidationError> {
        let tail = TAIL.replace("  work:\n", &format!("  work:\n{step_workspace}"));
        validate_source(&format!("{header}{tail}")).expect("the file must parse")
    }

    const REPOS: &str = r#"
schema: v2
name: multi
version: 1
globals:
  repositories:
    - { name: code, primary: true, default_branch: main, repository: code-repo }
    - { name: docs, default_branch: main, repository: docs-repo }
"#;

    fn workspace_errors(section: &str) -> Vec<ValidationError> {
        errors_of(REPOS, &format!("    workspace: {section}\n"))
    }

    fn rejected_field(errors: &[ValidationError], expected: &str) -> bool {
        errors.iter().any(|error| {
            matches!(error, ValidationError::InvalidStepWorkspace { state, field, .. }
                if state == "work" && *field == expected)
        })
    }

    #[test]
    fn a_valid_v2_definition_has_no_errors() {
        assert!(workspace_errors("{ root: docs, write: [docs], read: [code] }").is_empty());
        assert!(errors_of(REPOS, "").is_empty());
    }

    #[test]
    fn unknown_names_in_root_write_and_read_name_the_step_and_field() {
        assert!(rejected_field(
            &workspace_errors("{ write: [nope] }"),
            "write"
        ));
        assert!(rejected_field(
            &workspace_errors("{ read: [nope] }"),
            "read"
        ));
        assert!(rejected_field(
            &workspace_errors("{ root: nope, read: [code] }"),
            "root"
        ));
    }

    #[test]
    fn a_repository_cannot_be_both_written_and_read() {
        assert!(rejected_field(
            &workspace_errors("{ write: [code], read: [code] }"),
            "read"
        ));
    }

    #[test]
    fn root_must_be_listed_or_the_parent_directory() {
        assert!(rejected_field(
            &workspace_errors("{ root: docs, write: [code] }"),
            "root"
        ));
        assert!(workspace_errors("{ root: \".\", write: [code] }").is_empty());
    }

    #[test]
    fn an_empty_workspace_section_is_rejected() {
        assert!(rejected_field(&workspace_errors("{}"), "write"));
    }

    #[test]
    fn root_dot_is_allowed_with_a_single_repository() {
        let header = "schema: v2\nname: one\nversion: 1\nglobals:\n  repositories:\n    - { name: only, primary: true, default_branch: main, path: ../only }\n";
        assert!(errors_of(header, "    workspace: { root: \".\", write: [only] }\n").is_empty());
    }

    fn repository_errors(repositories: &str) -> Vec<String> {
        let header = format!(
            "schema: v2\nname: r\nversion: 1\ninputs:\n  - {{ name: repo, type: repository }}\nglobals:\n  repositories:\n{repositories}"
        );
        errors_of(&header, "")
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn zero_or_two_primary_repositories_are_rejected() {
        let none = repository_errors(
            "    - { name: a, default_branch: main, repository: r }\n    - { name: b, default_branch: main, repository: r }\n",
        );
        assert!(
            none.iter()
                .any(|e| e.contains("exactly one repository must be primary, found 0")),
            "{none:?}"
        );
        let two = repository_errors(
            "    - { name: a, primary: true, default_branch: main, repository: r }\n    - { name: b, primary: true, default_branch: main, repository: r }\n",
        );
        assert!(two.iter().any(|e| e.contains("found 2")), "{two:?}");
    }

    #[test]
    fn repository_names_must_be_usable_and_unique() {
        let errors = repository_errors(
            "    - { name: a, primary: true, default_branch: main, repository: r }\n    - { name: a, default_branch: main, repository: r }\n    - { name: \"x/y\", default_branch: main, repository: r }\n",
        );
        assert!(errors.iter().any(|e| e.contains("globals.repositories[1].name") && e.contains("more than once")), "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("globals.repositories[2].name") && e.contains("usable")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_registered_v2_repository_needs_a_default_branch() {
        let errors = repository_errors("    - { name: a, primary: true, repository: r }\n");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("globals.repositories[0].default_branch")),
            "{errors:?}"
        );
    }

    #[test]
    fn an_undeclared_input_in_a_v2_source_names_the_v2_field() {
        let errors = repository_errors(
            "    - { name: a, primary: true, default_branch: main, repository: \"{{ inputs.ghost }}\" }\n",
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("globals.repositories[0].repository") && e.contains("ghost")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_v1_error_names_the_original_field() {
        let yaml = format!(
            "name: legacy\nversion: 1\nglobals:\n  workspace:\n    type: git\n    repository: \"{{{{ inputs.ghost }}}}\"\n{TAIL}"
        );
        let errors: Vec<String> = validate_source(&yaml)
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("globals.workspace.repository") && e.contains("ghost")),
            "{errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.contains("repositories[")),
            "{errors:?}"
        );
    }
}
