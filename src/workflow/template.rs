//! Template substitution for `{{ inputs.* }}` and `{{ context.variables.* }}`.
//!
//! The scope is deliberately minimal (D6): the only things recognised inside
//! `{{ }}` are a workflow input (`inputs.NAME`) and a context variable
//! (`context.variables.NAME`, or the shorthand `context.NAME` used by
//! `WORKFLOW_SCHEMA.md` §3.3.D). There are no filters, expressions,
//! conditionals or loops — a filter such as `{{ context.x | default("y") }}`
//! is reported as an explicit error rather than silently ignored.
//!
//! Two further rules matter for debuggability:
//!
//! * An unknown variable is an error, never an empty string. A prompt handed
//!   to an agent with half a sentence missing is far harder to diagnose than
//!   a failed action naming the variable and the place it was used.
//! * The `display` section is the one exception to the first rule
//!   ([`TemplateScope::render_display`]): a title or link is shown while the
//!   task is still queued, before the variables it names exist, so there an
//!   unknown variable makes the whole string empty instead of an error.
//! * Substitution is single-pass. The rendered result is never rescanned, so
//!   a variable whose value itself contains `{{ ... }}` cannot cause
//!   recursion.
//!
//! Rendering happens lazily, immediately before an action runs, so that
//! `{{ context.variables.x }}` picks up the value current at that moment and
//! the workflow definition itself stays immutable (D31).

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::domain::{
    safe_link_url, ActionConfig, AgentActionConfig, Context, DisplayLink, DisplaySpec,
    HumanActionConfig, ScriptActionConfig, TaskDisplay, WaitActionConfig, EMPTY_LINK_TEXT,
};

/// Opening delimiter of a template placeholder.
const OPEN: &str = "{{";
/// Closing delimiter of a template placeholder.
const CLOSE: &str = "}}";

/// What rendering does about a variable that has no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unknown {
    /// An [`TemplateError::UnknownVariable`].
    Error,
    /// An empty result for the whole template.
    Blank,
}

/// A failure encountered while rendering a template.
///
/// Every variant carries a `location` — a short human-readable description of
/// where the template came from (for example `state 'implement', prompt`) —
/// because the same message is what the operator sees in the failed action.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template referred to an input or context variable that does not exist.
    #[error("unknown template variable '{reference}' in {location}")]
    UnknownVariable {
        /// The reference exactly as written inside `{{ }}`.
        reference: String,
        /// Where the template was used.
        location: String,
    },

    /// The template used a filter expression, which this engine does not support.
    #[error(
        "filters are not supported in templates: '{expression}' in {location}; \
         use a plain variable such as '{{{{ context.variables.NAME }}}}' and set \
         the fallback value in the workflow instead"
    )]
    FilterUnsupported {
        /// The full expression written inside `{{ }}`.
        expression: String,
        /// Where the template was used.
        location: String,
    },

    /// A `{{` was opened but never closed.
    #[error("unterminated '{{{{' in {location}: template placeholders must be closed with '}}}}'")]
    UnterminatedPlaceholder {
        /// Where the template was used.
        location: String,
    },

    /// The reference is syntactically fine but does not name a supported root.
    #[error(
        "unsupported template reference '{reference}' in {location}; \
         only '{{{{ inputs.NAME }}}}' and '{{{{ context.variables.NAME }}}}' \
         (shorthand '{{{{ context.NAME }}}}') are supported"
    )]
    UnsupportedReference {
        /// The reference exactly as written inside `{{ }}`.
        reference: String,
        /// Where the template was used.
        location: String,
    },

    /// Execution inputs were not a JSON object.
    #[error("workflow inputs must be a JSON object, got {actual}")]
    InvalidInputs {
        /// The JSON type that was supplied instead.
        actual: &'static str,
    },
}

/// The values a template is rendered against: workflow inputs plus the
/// variables of the live execution context.
///
/// The scope borrows both sides, so the caller keeps ownership of the context
/// and no snapshot is taken — the values read are the ones current when
/// [`TemplateScope::render`] is called.
#[derive(Debug, Clone, Copy)]
pub struct TemplateScope<'a> {
    inputs: &'a Map<String, Value>,
    variables: &'a HashMap<String, Value>,
}

impl<'a> TemplateScope<'a> {
    /// Builds a scope from an inputs object and an execution context.
    pub fn new(inputs: &'a Map<String, Value>, context: &'a Context) -> Self {
        Self {
            inputs,
            variables: &context.variables,
        }
    }

    /// Builds a scope from an inputs object and a bare variable map, for a
    /// caller that has no [`Context`] (a task that has not started).
    pub fn from_parts(
        inputs: &'a Map<String, Value>,
        variables: &'a HashMap<String, Value>,
    ) -> Self {
        Self { inputs, variables }
    }

    /// Builds a scope from `WorkflowExecution::inputs`, which is stored as an
    /// untyped [`Value`] and must therefore be checked to be an object.
    pub fn from_execution(inputs: &'a Value, context: &'a Context) -> Result<Self, TemplateError> {
        match inputs.as_object() {
            Some(object) => Ok(Self::new(object, context)),
            None => Err(TemplateError::InvalidInputs {
                actual: json_type_name(inputs),
            }),
        }
    }

    /// Renders one template string.
    ///
    /// `location` describes where the string came from and is repeated in any
    /// error, so it should be specific enough to find the offending line in
    /// the workflow file (for example `state 'create_pr', arg #1`).
    pub fn render(&self, template: &str, location: &str) -> Result<String, TemplateError> {
        self.render_with(template, location, Unknown::Error)
            .map(|rendered| rendered.unwrap_or_default())
    }

    /// Renders every template of a `display` section against this scope.
    ///
    /// Lenient, and infallible: a string that mentions a variable with no
    /// value yet, or cannot be rendered at all (a filter, an unclosed `{{`
    /// — the validator reports those), renders as empty. An empty title
    /// leaves the task's id to stand in for it; a link without a usable
    /// target has no URL, and one without text shows [`EMPTY_LINK_TEXT`].
    /// A URL that is not an `http(s)` one is dropped ([`safe_link_url`]), and
    /// control characters are stripped from the text ([`TaskDisplay::sanitized`]).
    pub fn render_display(&self, display: &DisplaySpec) -> TaskDisplay {
        let lenient = |template: &str| {
            self.render_with(template, "display", Unknown::Blank)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        let links = display
            .links
            .iter()
            .map(|link| {
                let url = safe_link_url(&lenient(&link.url));
                let text = match &link.text {
                    Some(text) => lenient(text),
                    None => url.clone().unwrap_or_default(),
                };
                DisplayLink {
                    label: link.label.clone(),
                    text: if text.is_empty() {
                        EMPTY_LINK_TEXT.to_string()
                    } else {
                        text
                    },
                    url,
                }
            })
            .collect();
        TaskDisplay {
            title: display.title.as_deref().map(lenient).unwrap_or_default(),
            links,
        }
        .sanitized()
    }

    /// The one renderer. `None` means [`Unknown::Blank`] met a variable
    /// without a value, so the whole result is empty.
    fn render_with(
        &self,
        template: &str,
        location: &str,
        unknown: Unknown,
    ) -> Result<Option<String>, TemplateError> {
        // Fast path: the overwhelming majority of strings carry no template.
        if !template.contains(OPEN) {
            return Ok(Some(template.to_string()));
        }

        let mut rendered = String::with_capacity(template.len());
        let mut rest = template;
        let mut complete = true;

        while let Some(start) = rest.find(OPEN) {
            rendered.push_str(&rest[..start]);
            let after_open = &rest[start + OPEN.len()..];

            let end =
                after_open
                    .find(CLOSE)
                    .ok_or_else(|| TemplateError::UnterminatedPlaceholder {
                        location: location.to_string(),
                    })?;

            let expression = &after_open[..end];
            match self.resolve(expression, location) {
                Ok(value) => rendered.push_str(&value),
                Err(TemplateError::UnknownVariable { .. }) if unknown == Unknown::Blank => {
                    complete = false;
                }
                Err(error) => return Err(error),
            }

            // Continue after the closing delimiter. What was just written is
            // never rescanned, which is what makes substitution single-pass.
            rest = &after_open[end + CLOSE.len()..];
        }

        rendered.push_str(rest);
        Ok(complete.then_some(rendered))
    }

    /// Renders every templated field of an action configuration, returning a
    /// new configuration with the placeholders replaced.
    ///
    /// Covered fields: the agent prompt, the script command, arguments,
    /// working directory and environment values, the human instruction, and
    /// every string inside a wait filter. The definition itself is left
    /// untouched (D31).
    pub fn render_action(
        &self,
        action: &ActionConfig,
        location: &str,
    ) -> Result<ActionConfig, TemplateError> {
        match action {
            ActionConfig::Agent(config) => Ok(ActionConfig::Agent(AgentActionConfig {
                prompt: self.render(&config.prompt, &format!("{location}, prompt"))?,
                ..config.clone()
            })),

            ActionConfig::Script(config) => {
                let command = self.render(&config.command, &format!("{location}, command"))?;

                let mut args = Vec::with_capacity(config.args.len());
                for (index, arg) in config.args.iter().enumerate() {
                    args.push(self.render(arg, &format!("{location}, arg #{index}"))?);
                }

                let working_dir = match &config.working_dir {
                    Some(dir) => Some(self.render(dir, &format!("{location}, working_dir"))?),
                    None => None,
                };

                let mut env = HashMap::with_capacity(config.env.len());
                for (key, value) in &config.env {
                    let rendered = self.render(value, &format!("{location}, env '{key}'"))?;
                    env.insert(key.clone(), rendered);
                }

                Ok(ActionConfig::Script(ScriptActionConfig {
                    command,
                    args,
                    working_dir,
                    env,
                    ..config.clone()
                }))
            }

            ActionConfig::Human(config) => Ok(ActionConfig::Human(HumanActionConfig {
                instruction: self.render(&config.instruction, &format!("{location}, message"))?,
                ..config.clone()
            })),

            ActionConfig::Wait(config) => {
                let filter = match &config.filter {
                    Some(filter) => {
                        Some(self.render_value(filter, &format!("{location}, filter"))?)
                    }
                    None => None,
                };

                Ok(ActionConfig::Wait(WaitActionConfig {
                    filter,
                    ..config.clone()
                }))
            }
        }
    }

    /// Renders every string inside a JSON value, recursing into arrays and
    /// objects. Object keys are left alone — only values are templated.
    pub fn render_value(&self, value: &Value, location: &str) -> Result<Value, TemplateError> {
        match value {
            Value::String(text) => Ok(Value::String(self.render(text, location)?)),

            Value::Array(items) => {
                let mut rendered = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    rendered.push(self.render_value(item, &format!("{location}[{index}]"))?);
                }
                Ok(Value::Array(rendered))
            }

            Value::Object(entries) => {
                let mut rendered = Map::new();
                for (key, item) in entries {
                    rendered.insert(
                        key.clone(),
                        self.render_value(item, &format!("{location}.{key}"))?,
                    );
                }
                Ok(Value::Object(rendered))
            }

            other => Ok(other.clone()),
        }
    }

    /// Resolves the contents of a single `{{ ... }}` placeholder.
    fn resolve(&self, expression: &str, location: &str) -> Result<String, TemplateError> {
        let reference = expression.trim();

        if reference.contains('|') {
            return Err(TemplateError::FilterUnsupported {
                expression: reference.to_string(),
                location: location.to_string(),
            });
        }

        let value = self.lookup(reference, location)?;
        Ok(render_scalar(value))
    }

    /// Looks a reference up in the scope, distinguishing "path I do not
    /// understand" from "path I understand but no such variable".
    fn lookup(&self, reference: &str, location: &str) -> Result<&'a Value, TemplateError> {
        let unknown = || TemplateError::UnknownVariable {
            reference: reference.to_string(),
            location: location.to_string(),
        };
        let unsupported = || TemplateError::UnsupportedReference {
            reference: reference.to_string(),
            location: location.to_string(),
        };

        let (root, path) = reference.split_once('.').ok_or_else(unsupported)?;

        match root {
            "inputs" => {
                let name = plain_name(path).ok_or_else(unsupported)?;
                self.inputs.get(name).ok_or_else(unknown)
            }

            "context" => {
                // Both `context.variables.NAME` and the `context.NAME`
                // shorthand from §3.3.D address the same map.
                let name = path.strip_prefix("variables.").unwrap_or(path);
                let name = plain_name(name).ok_or_else(unsupported)?;
                self.variables.get(name).ok_or_else(unknown)
            }

            _ => Err(unsupported()),
        }
    }
}

/// Checks the syntax of a template without resolving anything: the errors
/// rendering would give for a filter, an unclosed `{{` or an unsupported
/// reference, and none for a variable that merely has no value (yet).
pub fn check_syntax(template: &str, location: &str) -> Result<(), TemplateError> {
    let inputs = Map::new();
    let variables = HashMap::new();
    TemplateScope::from_parts(&inputs, &variables)
        .render_with(template, location, Unknown::Blank)
        .map(|_| ())
}

/// The context variables a template refers to, in first-use order and
/// without repeats (KTD15).
///
/// Static: nothing is resolved, so it answers "which `context.NAME` will this
/// template need" before any value exists. Only well-formed `context.NAME`
/// and `context.variables.NAME` references count; whatever [`TemplateScope`]
/// would refuse (a filter, an unterminated placeholder, an unsupported root)
/// is not a variable anyone could supply, so it is left for rendering to
/// report.
pub fn context_references(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    collect_context_references(template, &mut names);
    names
}

/// The context variables an action's templates refer to, over exactly the
/// fields [`TemplateScope::render_action`] renders, in first-use order and
/// without repeats.
pub fn action_context_references(action: &ActionConfig) -> Vec<String> {
    let mut names = Vec::new();
    match action {
        ActionConfig::Agent(config) => collect_context_references(&config.prompt, &mut names),
        ActionConfig::Script(config) => {
            collect_context_references(&config.command, &mut names);
            for arg in &config.args {
                collect_context_references(arg, &mut names);
            }
            if let Some(dir) = &config.working_dir {
                collect_context_references(dir, &mut names);
            }
            // Sorted, so the order does not depend on the map's.
            let mut keys: Vec<&String> = config.env.keys().collect();
            keys.sort();
            for key in keys {
                collect_context_references(&config.env[key], &mut names);
            }
        }
        ActionConfig::Human(config) => collect_context_references(&config.instruction, &mut names),
        ActionConfig::Wait(config) => {
            if let Some(filter) = &config.filter {
                collect_value_references(filter, &mut names);
            }
        }
    }
    names
}

/// Adds the context variables `template` refers to that `names` lacks.
fn collect_context_references(template: &str, names: &mut Vec<String>) {
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            return;
        };
        if let Some(name) = context_name(after_open[..end].trim()) {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
        rest = &after_open[end + CLOSE.len()..];
    }
}

/// [`collect_context_references`] over every string inside a JSON value.
fn collect_value_references(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::String(text) => collect_context_references(text, names),
        Value::Array(items) => {
            for item in items {
                collect_value_references(item, names);
            }
        }
        Value::Object(entries) => {
            for item in entries.values() {
                collect_value_references(item, names);
            }
        }
        _ => {}
    }
}

/// The variable a `context.NAME` / `context.variables.NAME` reference names.
fn context_name(reference: &str) -> Option<&str> {
    if reference.contains('|') || reference.contains(char::is_whitespace) {
        return None;
    }
    let path = reference.strip_prefix("context.")?;
    plain_name(path.strip_prefix("variables.").unwrap_or(path))
}

/// Accepts a bare variable name, rejecting empty names and nested paths — the
/// scope has no notion of drilling into a value.
fn plain_name(candidate: &str) -> Option<&str> {
    if candidate.is_empty() || candidate.contains('.') {
        None
    } else {
        Some(candidate)
    }
}

/// Renders a JSON value for inclusion in text.
///
/// A string is inserted as-is (no surrounding quotes, no escaping), numbers
/// and booleans as bare JSON scalars, and arrays and objects as compact JSON.
fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Name of a JSON value's type, for error messages.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
