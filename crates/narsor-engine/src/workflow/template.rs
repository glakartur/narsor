//! Template substitution for inputs, context variables and selected step results.
//!
//! The scope is deliberately minimal (D6): the only things recognised inside
//! `{{ }}` are a workflow input (`inputs.NAME`), a context variable
//! (`context.variables.NAME`, or the shorthand `context.NAME` used by
//! `docs/dev/workflow-schema.md` §3.3.D) and a field of its value when that is
//! an object (`context.variables.NAME.field`), a value of a workspace
//! (`context.workspace.NAME.field`, [`WorkspaceField`]), a named artifact (`artifacts.NAME`, the path
//! it was last written to, KTD7), selected step fields (`reply`,
//! `stdout`, `stderr`), the current step's own memory (`local.NAME`, R13)
//! and the history of its earlier attempts (`step.history`, R18). There are
//! no filters, expressions,
//! conditionals or loops — a filter such as `{{ context.x | default("y") }}`
//! is reported as an explicit error rather than silently ignored.
//!
//! Two further rules matter for debuggability:
//!
//! * An unknown variable is an error, never an empty string. A prompt handed
//!   to an agent with half a sentence missing is far harder to diagnose than
//!   a failed action naming the variable and the place it was used. A name
//!   the workflow declares ([`DeclaredNames`]) is not unknown, only unset:
//!   it renders as empty text (KTD6).
//! * Substitution is single-pass. The rendered result is never rescanned, so
//!   a variable whose value itself contains `{{ ... }}` cannot cause
//!   recursion.
//!
//! Rendering happens lazily, immediately before an action runs, so that
//! `{{ context.variables.x }}` picks up the value current at that moment and
//! the workflow definition itself stays immutable (D31).

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::domain::{
    ActionConfig, AgentActionConfig, ArtifactReference, Context, DecisionActionConfig,
    HumanActionConfig, PromptPart, PromptText, ScriptActionConfig, VariableDefinition,
    WaitActionConfig, WorkflowDefinition, WorkspaceField, WorkspacePr, LOCAL_VARIABLE_PREFIX,
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
         supported roots are inputs, context, artifacts, steps, local and step"
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

/// The workspace values the runner owns: the repository, the branches and the
/// size of the change. The engine only reads them, when a template is
/// rendered; it never stores them.
///
/// A value is the last one the runner read, which can be older than the step
/// that just ended.
pub trait WorkspaceValues: std::fmt::Debug + Send + Sync {
    /// The value of `field` of the workspace `workspace`, `None` when it has
    /// none. The scope has checked the workspace name, and answers `pr` and
    /// `pr_url` itself.
    fn value(&self, workspace: &str, field: WorkspaceField) -> Option<Value>;
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
    artifacts: Option<&'a HashMap<String, ArtifactReference>>,
    declared: Option<&'a DeclaredNames>,
    workspace_prs: Option<&'a BTreeMap<String, WorkspacePr>>,
    workspace_values: Option<&'a dyn WorkspaceValues>,
    step_replies: Option<&'a HashMap<String, Value>>,
    step_streams: Option<&'a HashMap<String, Value>>,
    step_labels: Option<&'a HashMap<String, Value>>,
    step_memory: Option<StepMemory<'a>>,
    task_ticket: &'a str,
    task_title: &'a str,
}

/// What the step being rendered keeps for itself: its local values, the
/// local variables it defines, and the history of its earlier attempts.
#[derive(Debug, Clone, Copy)]
pub struct StepMemory<'a> {
    /// The values kept from earlier attempts, keyed by the declared name
    /// (`memory`, or `local.memory` in the older shape).
    pub locals: &'a Map<String, Value>,
    /// The step's variable definitions: a local one without a value renders
    /// as empty text (R14).
    pub definitions: &'a [VariableDefinition],
    /// The history block, empty when the step gets none (R17, R19).
    pub history: &'a str,
}

impl StepMemory<'_> {
    /// The local variable `{{ local.NAME }}` reads: declared with
    /// `scope: local` as `NAME`, or as `local.NAME` in the older shape.
    fn declared_local(&self, name: &str) -> Option<&VariableDefinition> {
        self.definitions.iter().find(|variable| {
            variable.is_local()
                && variable
                    .name
                    .strip_prefix(LOCAL_VARIABLE_PREFIX)
                    .unwrap_or(&variable.name)
                    == name
        })
    }

    fn local(&self, name: &str) -> Option<&Value> {
        match self.declared_local(name) {
            Some(variable) => self.locals.get(&variable.name),
            None => self.locals.get(&format!("{LOCAL_VARIABLE_PREFIX}{name}")),
        }
    }

    fn defines_local(&self, name: &str) -> bool {
        self.declared_local(name).is_some()
    }
}

/// The context variables and artifacts a workflow declares: the names whose
/// missing value renders as empty text rather than as an unknown variable
/// (KTD6).
///
/// A context variable is declared by a step that defines it, or by a gate
/// that asks for a field of that name; an artifact by a step that defines
/// it; a workspace by the workflow's repositories. A step's own `local.*`
/// variables are not context variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredNames {
    variables: HashSet<String>,
    artifacts: HashSet<String>,
    workspaces: HashSet<String>,
}

impl DeclaredNames {
    /// The names `definition` declares.
    pub fn of(definition: &WorkflowDefinition) -> Self {
        let mut declared = Self::default();
        declared.workspaces.extend(
            definition
                .repositories
                .iter()
                .map(|repository| repository.name.clone()),
        );
        for state in definition.states.values() {
            let step_variables = state
                .variables
                .iter()
                .filter(|variable| !variable.is_local())
                .map(|variable| variable.name.clone());
            declared.variables.extend(step_variables);
            if let Some(ActionConfig::Human(gate)) = &state.action {
                declared
                    .variables
                    .extend(gate.inputs.iter().map(|field| field.name.clone()));
            }
            declared
                .artifacts
                .extend(state.artifacts.iter().map(|artifact| artifact.name.clone()));
        }
        declared
    }

    /// Whether some step or gate declares the context variable `name`.
    pub fn declares_variable(&self, name: &str) -> bool {
        self.variables.contains(name)
    }

    /// Whether the workflow declares the workspace `name`.
    pub fn declares_workspace(&self, name: &str) -> bool {
        self.workspaces.contains(name)
    }

    /// Whether some step defines the artifact `name`.
    pub fn declares_artifact(&self, name: &str) -> bool {
        self.artifacts.contains(name)
    }
}

impl<'a> TemplateScope<'a> {
    /// Builds a scope from an inputs object and an execution context.
    pub fn new(inputs: &'a Map<String, Value>, context: &'a Context) -> Self {
        Self {
            inputs,
            variables: &context.variables,
            artifacts: Some(&context.artifacts),
            declared: None,
            workspace_prs: Some(&context.workspace_prs),
            workspace_values: None,
            step_replies: None,
            step_streams: None,
            step_labels: None,
            step_memory: None,
            task_ticket: "",
            task_title: "",
        }
    }

    /// Builds a scope from an inputs object and a bare variable map, for a
    /// caller that has no [`Context`] (a task that has not started).
    pub fn from_parts(
        inputs: &'a Map<String, Value>,
        variables: &'a HashMap<String, Value>,
    ) -> Self {
        Self {
            inputs,
            variables,
            artifacts: None,
            declared: None,
            workspace_prs: None,
            workspace_values: None,
            step_replies: None,
            step_streams: None,
            step_labels: None,
            step_memory: None,
            task_ticket: "",
            task_title: "",
        }
    }

    /// Adds the names the workflow declares, so that a declared variable or
    /// artifact without a value renders as empty text (KTD6).
    pub fn with_declared(mut self, declared: &'a DeclaredNames) -> Self {
        self.declared = Some(declared);
        self
    }

    /// Adds the workspace values the runner owns: `{{ context.workspace.NAME.FIELD }}`
    /// for every field but `pr` and `pr_url`. A scope without them reads
    /// those fields as empty text.
    pub fn with_workspace_values(mut self, values: &'a dyn WorkspaceValues) -> Self {
        self.workspace_values = Some(values);
        self
    }

    /// Adds persisted replies keyed by workflow step id.
    pub fn with_step_replies(mut self, replies: &'a HashMap<String, Value>) -> Self {
        self.step_replies = Some(replies);
        self
    }

    /// Adds captured script streams keyed by `step.field`.
    pub fn with_step_streams(mut self, streams: &'a HashMap<String, Value>) -> Self {
        self.step_streams = Some(streams);
        self
    }

    /// Adds the label each step last chose, keyed by workflow step id:
    /// `{{ steps.ID.label }}`.
    pub fn with_step_labels(mut self, labels: &'a HashMap<String, Value>) -> Self {
        self.step_labels = Some(labels);
        self
    }

    /// Adds the memory of the step being rendered: `{{ local.NAME }}` and
    /// `{{ step.history }}`.
    pub fn with_step_memory(mut self, memory: StepMemory<'a>) -> Self {
        self.step_memory = Some(memory);
        self
    }

    /// Adds the task's own ticket and title: `{{ task.ticket }}` and
    /// `{{ task.title }}`. A task without one renders as empty text, and a
    /// scope that never got them reads both as empty.
    pub fn with_task_fields(mut self, ticket: Option<&'a str>, title: Option<&'a str>) -> Self {
        self.task_ticket = ticket.unwrap_or_default();
        self.task_title = title.unwrap_or_default();
        self
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

    /// Renders a prompt or message: each part on its own, a part whose
    /// `when` has an empty reference dropped, the rest joined by an empty
    /// line into a text of one part.
    ///
    /// A text of several parts names the part in an error: `location, part 2`.
    pub fn render_text(
        &self,
        text: &PromptText,
        location: &str,
    ) -> Result<PromptText, TemplateError> {
        let numbered = text.parts().len() > 1;
        let mut rendered = Vec::new();
        for (index, part) in text.parts().iter().enumerate() {
            let place = if numbered {
                format!("{location}, part {}", index + 1)
            } else {
                location.to_string()
            };
            if !self.part_applies(part, &place)? {
                continue;
            }
            rendered.push(self.render(&part.text, &place)?);
        }
        Ok(PromptText::plain(rendered.join(PromptText::PART_SEPARATOR)))
    }

    /// Whether every reference of the part's `when` has a value.
    fn part_applies(&self, part: &PromptPart, location: &str) -> Result<bool, TemplateError> {
        let place = format!("{location}, when");
        for reference in &part.when {
            if self.is_empty_reference(reference, &place)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether `reference` names no value: missing, `null`, or text of
    /// nothing but white space. `false` and `0` are values. The result of a
    /// step that has not run is missing, not an error.
    fn is_empty_reference(&self, reference: &str, location: &str) -> Result<bool, TemplateError> {
        let reference = reference.trim();
        match self.resolve_value(reference, location) {
            Ok(Value::Null) => Ok(true),
            Ok(Value::String(text)) => Ok(text.trim().is_empty()),
            Ok(_) => Ok(false),
            Err(TemplateError::UnknownVariable { .. }) if reference.starts_with("steps.") => {
                Ok(true)
            }
            Err(error) => Err(error),
        }
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
                prompt: self.render_text(&config.prompt, &format!("{location}, prompt"))?,
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

            ActionConfig::Decision(config) => Ok(ActionConfig::Decision(DecisionActionConfig {
                message: self.render_text(&config.message, &format!("{location}, message"))?,
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
        self.resolve_value(expression, location)
            .map(|value| render_scalar(&value))
    }

    /// [`Self::resolve`] without turning the value into text.
    fn resolve_value(&self, expression: &str, location: &str) -> Result<Value, TemplateError> {
        let reference = expression.trim();

        if reference.contains('|') {
            return Err(TemplateError::FilterUnsupported {
                expression: reference.to_string(),
                location: location.to_string(),
            });
        }

        self.lookup(reference, location)
    }

    /// Looks a reference up in the scope and renders its value, distinguishing
    /// "path I do not understand" from "path I understand but no such
    /// variable".
    fn lookup(&self, reference: &str, location: &str) -> Result<Value, TemplateError> {
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
                self.inputs.get(name).cloned().ok_or_else(unknown)
            }

            "context" => match parse_context_path(path).ok_or_else(unsupported)? {
                ContextPath::Variable { name, fields } => {
                    let base = match self.variables.get(name) {
                        Some(value) => value.clone(),
                        None if self.declares_variable(name) && fields.is_empty() => {
                            Value::String(String::new())
                        }
                        None => return Err(unknown()),
                    };
                    fields.iter().try_fold(base, |value, field| match value {
                        Value::Object(mut entries) => entries.remove(*field).ok_or_else(unknown),
                        _ => Err(unknown()),
                    })
                }
                ContextPath::Workspace { name, field } => {
                    let field = WorkspaceField::from_key(field).ok_or_else(unknown)?;
                    if !self.declares_workspace(name) {
                        return Err(unknown());
                    }
                    Ok(self
                        .workspace_value(name, field)
                        .unwrap_or_else(|| Value::String(String::new())))
                }
            },

            "artifacts" => {
                let name = plain_name(path).ok_or_else(unsupported)?;
                match self.artifact_location(name) {
                    Some(location) => Ok(Value::String(location.to_string())),
                    None if self.declares_artifact(name) => Ok(Value::String(String::new())),
                    None => Err(unknown()),
                }
            }

            "steps" => {
                let (step, field) = path.split_once('.').ok_or_else(unsupported)?;
                if plain_name(step).is_none() {
                    return Err(unsupported());
                }
                match field {
                    "reply" => self.step_replies.and_then(|replies| replies.get(step)),
                    "label" => self.step_labels.and_then(|labels| labels.get(step)),
                    "stdout" | "stderr" => self
                        .step_streams
                        .and_then(|streams| streams.get(&format!("{step}.{field}"))),
                    _ => return Err(unsupported()),
                }
                .cloned()
                .ok_or_else(unknown)
            }

            "local" => {
                let name = plain_name(path).ok_or_else(unsupported)?;
                let memory = self.step_memory.ok_or_else(unknown)?;
                match memory.local(name) {
                    Some(value) => Ok(value.clone()),
                    None if memory.defines_local(name) => Ok(Value::String(String::new())),
                    None => Err(unknown()),
                }
            }

            "task" => match path {
                "ticket" => Ok(Value::String(self.task_ticket.to_string())),
                "title" => Ok(Value::String(self.task_title.to_string())),
                _ => Err(unknown()),
            },

            "step" if path == "history" => Ok(Value::String(
                self.step_memory
                    .map(|memory| memory.history.to_string())
                    .unwrap_or_default(),
            )),

            _ => Err(unsupported()),
        }
    }

    /// Where the artifact `name` was last written. Only a named artifact
    /// counts: an unnamed one is kept under its id, which is not a name a
    /// workflow could have defined.
    fn artifact_location(&self, name: &str) -> Option<&'a str> {
        self.artifacts?
            .get(name)
            .filter(|artifact| artifact.name.as_deref() == Some(name))
            .map(|artifact| artifact.location.as_str())
    }

    /// The value of `field` of the declared workspace `name`: the pull
    /// request values from the context, the others from the runner.
    fn workspace_value(&self, name: &str, field: WorkspaceField) -> Option<Value> {
        match field {
            WorkspaceField::Pr | WorkspaceField::PrUrl => {
                let pr = self.workspace_prs?.get(name)?;
                let text = if field == WorkspaceField::Pr {
                    &pr.pr
                } else {
                    &pr.pr_url
                };
                text.clone().map(Value::String)
            }
            _ => self.workspace_values?.value(name, field),
        }
    }

    fn declares_workspace(&self, name: &str) -> bool {
        self.declared
            .is_some_and(|declared| declared.declares_workspace(name))
    }

    fn declares_variable(&self, name: &str) -> bool {
        self.declared
            .is_some_and(|declared| declared.declares_variable(name))
    }

    fn declares_artifact(&self, name: &str) -> bool {
        self.declared
            .is_some_and(|declared| declared.declares_artifact(name))
    }
}

/// Checks the syntax of a template without resolving anything: the errors
/// rendering would give for a filter, an unclosed `{{` or an unsupported
/// reference, and none for a variable that merely has no value (yet).
pub fn check_syntax(template: &str, location: &str) -> Result<(), TemplateError> {
    let inputs = Map::new();
    let variables = HashMap::new();
    let replies = step_reply_references(template)
        .into_iter()
        .map(|id| (id, Value::String(String::new())))
        .collect();
    let streams = step_stream_references(template)
        .into_iter()
        .map(|(id, field)| (format!("{id}.{field}"), Value::String(String::new())))
        .collect();
    let labels = step_label_references(template)
        .into_iter()
        .map(|id| (id, Value::String(String::new())))
        .collect();
    TemplateScope::from_parts(&inputs, &variables)
        .with_step_replies(&replies)
        .with_step_streams(&streams)
        .with_step_labels(&labels)
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

/// The `context.workspace…` references of a template that cannot be read, as
/// `(reference, reason)` in order of use: a form other than
/// `context.workspace.<name>.<field>`, a workspace `declared` does not know,
/// or a field outside [`WorkspaceField`]. Nothing is resolved.
pub fn workspace_reference_problems(
    template: &str,
    declared: &DeclaredNames,
) -> Vec<(String, String)> {
    let mut problems = Vec::new();
    for expression in expressions(template) {
        let Some(path) = expression.strip_prefix("context.") else {
            continue;
        };
        if expression.contains('|') || path.split('.').next() != Some("workspace") {
            continue;
        }
        let reason = match parse_context_path(path) {
            Some(ContextPath::Workspace { name, field }) => {
                if !declared.declares_workspace(name) {
                    format!("the workflow declares no workspace '{name}'")
                } else if WorkspaceField::from_key(field).is_none() {
                    format!(
                        "'{field}' is not a workspace field (fields: {})",
                        WorkspaceField::all_keys()
                    )
                } else {
                    continue;
                }
            }
            _ => "write it as 'context.workspace.<name>.<field>'".to_string(),
        };
        problems.push((expression.to_string(), reason));
    }
    problems
}

/// The artifact names a template refers to through `artifacts.NAME`, in
/// first-use order and without repeats (KTD7).
pub fn artifact_references(template: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for expression in expressions(template) {
        let name = expression
            .strip_prefix("artifacts.")
            .and_then(plain_name)
            .filter(|name| !name.contains(char::is_whitespace));
        if let Some(name) = name {
            if !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Whether a template places the step's history itself (`{{ step.history }}`,
/// R18), so that the engine does not add it a second time.
pub fn places_step_history(template: &str) -> bool {
    expressions(template).contains(&"step.history")
}

/// Whether a template embeds content the user supplied: a task input
/// (`inputs.*`) or the task's own ticket and title (`task.*`).
pub fn embeds_user_content(template: &str) -> bool {
    expressions(template)
        .into_iter()
        .any(|expression| expression.starts_with("inputs.") || expression.starts_with("task."))
}

/// Workflow step ids referenced through `steps.<id>.reply`, in first-use order.
pub fn step_reply_references(template: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for path in step_paths(template) {
        if let Some((id, "reply")) = path.split_once('.') {
            if plain_name(id).is_some() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// Workflow step ids referenced through `steps.<id>.label`, in first-use order.
pub fn step_label_references(template: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for path in step_paths(template) {
        if let Some((id, "label")) = path.split_once('.') {
            if plain_name(id).is_some() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// Script stream references in first-use order, without repeats.
pub fn step_stream_references(template: &str) -> Vec<(String, String)> {
    let mut references = Vec::new();
    for path in step_paths(template) {
        if let Some((id, field @ ("stdout" | "stderr"))) = path.split_once('.') {
            if plain_name(id).is_some()
                && !references.iter().any(|(existing_id, existing_field)| {
                    existing_id == id && existing_field == field
                })
            {
                references.push((id.to_string(), field.to_string()));
            }
        }
    }
    references
}

/// The `steps.*` expressions of a template that are not supported step fields
/// (`steps.a.data`, `steps.a`), in order of use.
pub fn unsupported_step_references(template: &str) -> Vec<String> {
    step_paths(template)
        .into_iter()
        .filter(
            |path| !matches!(path.split_once('.'), Some((id, "reply" | "stdout" | "stderr" | "label")) if plain_name(id).is_some()),
        )
        .map(|path| format!("steps.{path}"))
        .collect()
}

/// What follows `steps.` in each `{{ steps.… }}` expression of a template.
fn step_paths(template: &str) -> Vec<&str> {
    expressions(template)
        .into_iter()
        .filter_map(|expression| expression.strip_prefix("steps."))
        .collect()
}

/// The trimmed contents of each closed `{{ … }}` of a template, in order.
fn expressions(template: &str) -> Vec<&str> {
    let mut expressions = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find(OPEN) {
        let after_open = &rest[start + OPEN.len()..];
        let Some(end) = after_open.find(CLOSE) else {
            break;
        };
        expressions.push(after_open[..end].trim());
        rest = &after_open[end + CLOSE.len()..];
    }
    expressions
}

/// The context variables an action's templates refer to, over exactly the
/// fields [`TemplateScope::render_action`] renders, in first-use order and
/// without repeats.
pub fn action_context_references(action: &ActionConfig) -> Vec<String> {
    let mut names = Vec::new();
    match action {
        ActionConfig::Agent(config) => {
            for piece in config.prompt.template_pieces() {
                collect_context_references(&piece, &mut names);
            }
        }
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
        ActionConfig::Decision(config) => {
            for piece in config.message.template_pieces() {
                collect_context_references(&piece, &mut names);
            }
        }
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

/// The variable a `context.NAME`, `context.variables.NAME` or
/// `context.variables.NAME.field` reference names. A workspace reference
/// names no variable.
fn context_name(reference: &str) -> Option<&str> {
    if reference.contains('|') || reference.contains(char::is_whitespace) {
        return None;
    }
    match parse_context_path(reference.strip_prefix("context.")?)? {
        ContextPath::Variable { name, .. } => Some(name),
        ContextPath::Workspace { .. } => None,
    }
}

/// What a `context.` reference reads.
#[derive(Debug, PartialEq, Eq)]
enum ContextPath<'a> {
    /// A variable and, when its value is an object, the fields to descend.
    Variable { name: &'a str, fields: Vec<&'a str> },
    /// The value `field` of the workspace `name`; the field is not checked
    /// against [`WorkspaceField`] here.
    Workspace { name: &'a str, field: &'a str },
}

/// Reads what follows `context.` in a reference: `workspace.NAME.FIELD`, or a
/// variable as `variables.NAME…` or the shorthand `NAME…`. `None` when the
/// text is not one of those forms.
fn parse_context_path(path: &str) -> Option<ContextPath<'_>> {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    match segments.as_slice() {
        ["workspace", name, field] => Some(ContextPath::Workspace { name, field }),
        ["workspace", ..] => None,
        ["variables", name, fields @ ..] => Some(ContextPath::Variable {
            name,
            fields: fields.to_vec(),
        }),
        ["variables"] => None,
        [name, fields @ ..] => Some(ContextPath::Variable {
            name,
            fields: fields.to_vec(),
        }),
        [] => None,
    }
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
pub(crate) fn render_scalar(value: &Value) -> String {
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

#[cfg(test)]
mod user_content_tests {
    use super::embeds_user_content;

    #[test]
    fn only_inputs_and_task_count_as_user_content() {
        assert!(embeds_user_content("Do {{ inputs.description }}"));
        assert!(embeds_user_content("{{ task.title }}"));
        assert!(!embeds_user_content("Implement {{ context.summary }}"));
        assert!(!embeds_user_content("Implement the feature."));
    }
}
