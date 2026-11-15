//! What a variable's declaration says about the values it may take, and what
//! a label's `set` assigns when the label is taken.
//!
//! A value a step returns is held to the declared `options`. A value `set`
//! assigns is held to the declared `type` as well, and is rendered as a
//! template first.

use super::*;
use crate::domain::{
    ChoiceDefinition, VariableDefinition, VariableSource, VariableType, WorkspaceField,
};
use crate::workflow::template::render_scalar;

/// One variable a label's `set` assigns, with its value rendered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Assignment {
    /// The declared name of the variable.
    pub(crate) name: String,
    pub(crate) value: Value,
    /// Whether the variable is the step's own memory.
    pub(crate) local: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkspaceAssignment {
    pub(crate) workspace: String,
    pub(crate) field: WorkspaceField,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RenderedSet {
    pub(crate) variables: Vec<Assignment>,
    pub(crate) workspaces: Vec<WorkspaceAssignment>,
}

/// Why the first variable `variables` gives is not one of its declared
/// `options` — `None` when every value is allowed.
///
/// A blank value counts as not given, and so does a variable the workflow
/// alone sets: neither is the step's to write.
pub(crate) fn value_outside_options(
    state: &State,
    variables: &Map<String, Value>,
) -> Option<String> {
    variables.iter().find_map(|(name, value)| {
        let declared = state.variable(name)?;
        if declared.source != VariableSource::Step || human::is_blank(value) {
            return None;
        }
        options_problem(declared, value)
    })
}

/// The assignments `set` of the choice `label` makes on leaving `state`,
/// every value rendered by `scope` and checked against its declaration.
///
/// Nothing is written here: the caller writes all of them or, on an error,
/// none.
pub(crate) fn render_assignments(
    state: &State,
    label: &str,
    scope: &TemplateScope<'_>,
) -> Result<RenderedSet, String> {
    let Some(choice) = state.choices.iter().find(|choice| choice.label == label) else {
        return Ok(RenderedSet::default());
    };
    let globals = choice
        .set
        .global
        .iter()
        .map(|(name, value)| (name, value, false));
    let locals = choice
        .set
        .local
        .iter()
        .map(|(name, value)| (name, value, true));
    let variables = globals
        .chain(locals)
        .map(|(name, value, local)| render_assignment(state, label, scope, name, value, local))
        .collect::<Result<_, _>>()?;
    let workspaces = render_workspace_assignments(state, label, choice, scope)?;
    Ok(RenderedSet {
        variables,
        workspaces,
    })
}

fn render_workspace_assignments(
    state: &State,
    label: &str,
    choice: &ChoiceDefinition,
    scope: &TemplateScope<'_>,
) -> Result<Vec<WorkspaceAssignment>, String> {
    let mut assignments = Vec::new();
    for (workspace, values) in &choice.set.workspace {
        for (key, value) in values {
            let Some(field) = WorkspaceField::from_key(key).filter(|field| field.writable()) else {
                return Err(format!(
                    "'set' of '{label}' sets 'workspace.{workspace}.{key}'; only {} can be set",
                    WorkspaceField::writable_keys()
                ));
            };
            let location = format!(
                "state '{}', set 'workspace.{workspace}.{key}' of '{label}'",
                state.id
            );
            let rendered = scope
                .render_value(value, &location)
                .map_err(|error| error.to_string())?;
            let text = render_scalar(&rendered).trim().to_string();
            if !text.is_empty() {
                assignments.push(WorkspaceAssignment {
                    workspace: workspace.clone(),
                    field,
                    value: text,
                });
            }
        }
    }
    Ok(assignments)
}

fn render_assignment(
    state: &State,
    label: &str,
    scope: &TemplateScope<'_>,
    name: &str,
    value: &Value,
    local: bool,
) -> Result<Assignment, String> {
    let declared = state.variable(name).ok_or_else(|| {
        format!(
            "'set' of '{label}' names variable '{name}', which step '{}' does not declare",
            state.id
        )
    })?;
    if declared.is_local() != local {
        return Err(format!(
            "'set' of '{label}' assigns variable '{name}' as {}, but it is declared {}",
            scope_name(local),
            scope_name(declared.is_local()),
        ));
    }
    let location = format!("state '{}', set '{name}' of '{label}'", state.id);
    let value = scope
        .render_value(value, &location)
        .map_err(|error| error.to_string())?;
    if let Some(problem) =
        options_problem(declared, &value).or_else(|| type_problem(declared, &value))
    {
        return Err(format!("'set' of '{label}': {problem}"));
    }
    Ok(Assignment {
        name: declared.name.clone(),
        value,
        local,
    })
}

fn scope_name(local: bool) -> &'static str {
    if local {
        "local"
    } else {
        "global"
    }
}

fn options_problem(declared: &VariableDefinition, value: &Value) -> Option<String> {
    if declared.options.is_empty() || declared.options.contains(&render_scalar(value)) {
        return None;
    }
    Some(format!(
        "variable '{}' has the value '{}', which is not one of: {}",
        declared.name,
        render_scalar(value),
        declared.options.join(", ")
    ))
}

fn type_problem(declared: &VariableDefinition, value: &Value) -> Option<String> {
    let fits = match (declared.variable_type?, value) {
        (_, Value::Array(_) | Value::Object(_) | Value::Null) => false,
        (VariableType::Integer, Value::Number(number)) => number.is_i64() || number.is_u64(),
        (VariableType::Integer, Value::String(text)) => text.trim().parse::<i64>().is_ok(),
        (VariableType::Integer, _) => false,
        (VariableType::Boolean, Value::Bool(_)) => true,
        (VariableType::Boolean, Value::String(text)) => matches!(text.trim(), "true" | "false"),
        (VariableType::Boolean, _) => false,
        (VariableType::String | VariableType::Text | VariableType::Select, _) => true,
    };
    if fits {
        return None;
    }
    Some(format!(
        "variable '{}' has the value '{}', which does not fit its type",
        declared.name,
        render_scalar(value)
    ))
}
