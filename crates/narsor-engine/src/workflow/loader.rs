//! Compilation of a YAML workflow file into the domain model.
//!
//! The loader reads the file, deserializes it into [`crate::workflow::schema`]
//! structures and then compiles those into a [`WorkflowDefinition`]:
//!
//! * `globals.timeout` and `globals.retry_policy` are propagated as per-state
//!   defaults; a value declared on the state itself always wins,
//! * `terminal_states` is turned into the `terminal` flag on each named state,
//!   together with the `terminal_status` the entry declares (§3.5),
//! * `initial_state` is the state flagged `initial: true`, or — with no such
//!   flag — the first state in file order,
//! * repositories belong to an execution, not to a state: `globals.workspaces`
//!   (`schema: v2`) becomes the definition's repository list, and a `v1` file is
//!   converted to the same shape — `globals.workspace` becomes one primary
//!   repository, and an agent's `workspace_access` becomes the step's
//!   `workspaces` section rooted in it (`read-only` lists the primary
//!   repository in `read`; `read-write` leaves it writable). The file on disk
//!   is never touched.
//!
//! * a `schema: v2` file written in the unified state shape (choices as a
//!   mapping, `exit`, `decision` actions) is compiled into the same model as
//!   the legacy shape: the `next` of each choice becomes a transition, and a
//!   linear step's `exit` becomes its only choice, labelled [`EXIT_LABEL`].
//!   A `schema: v2` file still in the legacy shape loads as before and is
//!   reported as deprecated, see [`LoadDiagnostic`].
//!
//! No graph rules are checked here: an entry of `terminal_states` that names a
//! state which does not exist is carried through without complaint, and
//! reporting it is the validator's job.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::workspace::provisioner::is_usable_directory_name;

use crate::domain::{
    ActionConfig, AgentActionConfig, AgentDecisionConfig, ArtifactDefinition, ChoiceDefinition,
    DecisionActionConfig, DecisionConfig, DefinitionShape, DisplayLinkSpec, DisplaySpec,
    GlobalsConfig, HumanActionConfig, HumanDecisionConfig, HumanInputField, HumanInputType,
    InputParameter, ParamType, RepositoryDefinition, RepositorySource, RetryPolicy, SchemaVersion,
    ScriptActionConfig, State, StepWorkspace, TerminalStatus, VariableDefinition, WaitActionConfig,
    WorkflowDefinition, WorkspaceAccess, EXIT_LABEL,
};

use super::schema::{
    ActionSchema, ChoiceSchema, DecisionSchema, DisplaySchema, ExitSchema, GlobalsSchema,
    HumanInputTypeSchema, InputSchema, ParamTypeSchema, RepositorySchema, RepositorySourceSchema,
    RetryPolicySchema, StateSchema, StateShape, StepWorkspaceSchema, TerminalStatusSchema,
    VariableSchema, WorkflowFile, WorkspaceSchema,
};

/// Errors that can occur while loading a workflow definition from YAML.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read workflow file '{path}'")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse workflow file '{path}': {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    #[error("failed to parse workflow definition: {0}")]
    ParseInline(#[source] serde_yaml::Error),

    #[error("workflow '{0}' declares no states")]
    NoStates(String),
}

/// Something a loaded file does that still works but should change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadDiagnostic {
    /// `schema: v2` with states in the legacy shape (`choices` as a list and
    /// a separate `transitions` map).
    #[error(
        "workflow '{workflow}' uses the legacy state shape under 'schema: v2' (choices as a \
         list with separate 'transitions'); it still loads, but the shape is deprecated: write \
         'choices' as a mapping with 'next' and use 'exit' for a linear step"
    )]
    DeprecatedStateShape { workflow: String },
}

/// A compiled definition with what its loading had to say.
#[derive(Debug, Clone)]
pub struct LoadedWorkflow {
    pub definition: WorkflowDefinition,
    pub diagnostics: Vec<LoadDiagnostic>,
}

/// Loads and compiles a workflow definition from a YAML string, returning the
/// diagnostics too.
pub fn load_from_str_with_diagnostics(yaml: &str) -> Result<LoadedWorkflow, LoadError> {
    let file: WorkflowFile = serde_yaml::from_str(yaml).map_err(LoadError::ParseInline)?;
    compile_with_diagnostics(file)
}

/// Loads and compiles a workflow definition from a YAML file.
pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<WorkflowDefinition, LoadError> {
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

    compile(file)
}

/// Loads and compiles a workflow definition from a YAML string.
pub fn load_from_str(yaml: &str) -> Result<WorkflowDefinition, LoadError> {
    let file: WorkflowFile = serde_yaml::from_str(yaml).map_err(LoadError::ParseInline)?;
    compile(file)
}

/// Compiles an already deserialized workflow file into the domain model.
///
/// Diagnostics are logged; [`compile_with_diagnostics`] returns them.
pub fn compile(file: WorkflowFile) -> Result<WorkflowDefinition, LoadError> {
    let loaded = compile_with_diagnostics(file)?;
    for diagnostic in &loaded.diagnostics {
        tracing::warn!("{diagnostic}");
    }
    Ok(loaded.definition)
}

/// Compiles an already deserialized workflow file into the domain model and
/// reports what is deprecated in it.
pub fn compile_with_diagnostics(file: WorkflowFile) -> Result<LoadedWorkflow, LoadError> {
    let mut diagnostics = Vec::new();
    if file.uses_deprecated_state_shape() {
        diagnostics.push(LoadDiagnostic::DeprecatedStateShape {
            workflow: file.name.clone(),
        });
    }
    let definition = compile_definition(file)?;
    Ok(LoadedWorkflow {
        definition,
        diagnostics,
    })
}

fn compile_definition(file: WorkflowFile) -> Result<WorkflowDefinition, LoadError> {
    if file.states.is_empty() {
        return Err(LoadError::NoStates(file.name));
    }

    let globals = file.globals.unwrap_or_default();
    let default_timeout_secs = globals.timeout.map(|d| d.as_secs());
    let default_retry_policy = globals.retry_policy.as_ref().map(compile_retry_policy);
    let repositories = compile_repositories(file.schema, &globals);
    let primary_name = repositories
        .iter()
        .find(|repository| repository.primary)
        .map(|repository| repository.name.as_str());

    let initial_state = file
        .states
        .iter()
        .find(|(_, state)| state.initial)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| file.states[0].0.clone());

    let mut states = HashMap::with_capacity(file.states.len());
    for (id, state) in file.states {
        let terminal = file.terminal_states.contains(&id);
        let terminal_status = file
            .terminal_states
            .status_of(&id)
            .map(compile_terminal_status);
        let compiled = compile_state(
            id.clone(),
            state,
            terminal,
            terminal_status,
            default_timeout_secs,
            default_retry_policy.as_ref(),
            primary_name,
        );
        states.insert(id, compiled);
    }

    Ok(WorkflowDefinition {
        // The YAML contract has no id; the name is declared unique, so it is
        // the only stable identifier available at load time.
        id: file.name.clone(),
        name: file.name,
        version: file.version,
        description: file.description,
        inputs: file.inputs.into_iter().map(compile_input).collect(),
        globals: compile_globals(&globals),
        source_schema: file.schema,
        shape: match file.shape {
            StateShape::Legacy => DefinitionShape::Legacy,
            StateShape::Unified => DefinitionShape::Unified,
        },
        repositories,
        display: file.display.map(compile_display),
        states,
        initial_state,
    })
}

fn compile_globals(globals: &GlobalsSchema) -> GlobalsConfig {
    GlobalsConfig {
        timeout_secs: globals.timeout.map(|d| d.as_secs()),
        retry_policy: globals.retry_policy.as_ref().map(compile_retry_policy),
        image: globals.image.clone(),
    }
}

fn compile_display(display: DisplaySchema) -> DisplaySpec {
    DisplaySpec {
        title: display.title,
        links: display
            .links
            .into_iter()
            .map(|link| DisplayLinkSpec {
                label: link.label,
                text: link.text,
                url: link.url,
            })
            .collect(),
    }
}

/// The repositories of a file: the declared list of a `v2` file, or the one
/// primary repository a `v1` `globals.workspace` converts to.
fn compile_repositories(
    schema: SchemaVersion,
    globals: &GlobalsSchema,
) -> Vec<RepositoryDefinition> {
    match schema {
        SchemaVersion::V2 => globals
            .workspaces
            .iter()
            .flatten()
            .map(compile_repository)
            .collect(),
        SchemaVersion::V1 => globals.workspace.iter().map(convert_v1_workspace).collect(),
    }
}

fn compile_repository(repository: &RepositorySchema) -> RepositoryDefinition {
    RepositoryDefinition {
        name: repository.name.clone(),
        primary: repository.primary,
        base_branch: repository.base_branch.clone(),
        source: match &repository.source {
            RepositorySourceSchema::Repository(repository) => RepositorySource::Git {
                repository: repository.clone(),
            },
            RepositorySourceSchema::Path(path) => RepositorySource::Local { path: path.clone() },
        },
        preserve_paths: repository.preserve_paths.clone(),
    }
}

/// Converts a `v1` `globals.workspace` into the primary repository. The
/// folder name comes from the registry id or the last path segment; a
/// templated or otherwise unusable value falls back to a fixed name, since the
/// real repository is only known once the task starts.
fn convert_v1_workspace(workspace: &WorkspaceSchema) -> RepositoryDefinition {
    match workspace {
        WorkspaceSchema::Git(git) => RepositoryDefinition {
            name: folder_name(&git.repository)
                .unwrap_or("repository")
                .to_string(),
            primary: true,
            base_branch: git.git_ref.clone(),
            source: RepositorySource::Git {
                repository: git.repository.clone(),
            },
            preserve_paths: git.preserve_paths.clone(),
        },
        WorkspaceSchema::Local(local) => RepositoryDefinition {
            name: local
                .path
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .and_then(folder_name)
                .unwrap_or("workspace")
                .to_string(),
            primary: true,
            base_branch: None,
            source: RepositorySource::Local {
                path: local.path.clone(),
            },
            preserve_paths: local.preserve_paths.clone(),
        },
    }
}

fn folder_name(candidate: &str) -> Option<&str> {
    (is_usable_directory_name(candidate) && !candidate.contains("{{")).then_some(candidate)
}

fn compile_retry_policy(policy: &RetryPolicySchema) -> RetryPolicy {
    RetryPolicy {
        max_attempts: policy.max_attempts,
        backoff_secs: policy.backoff.map(|d| d.as_secs()).unwrap_or(0),
        ..RetryPolicy::default()
    }
}

fn compile_input(input: InputSchema) -> InputParameter {
    InputParameter {
        name: input.name,
        param_type: match input.param_type {
            ParamTypeSchema::String => ParamType::String,
            ParamTypeSchema::Text => ParamType::Text,
            ParamTypeSchema::Integer => ParamType::Integer,
            ParamTypeSchema::Boolean => ParamType::Boolean,
            ParamTypeSchema::Repository => ParamType::Repository,
        },
        required: input.required,
        default: input.default,
        description: input.description,
    }
}

/// Maps a declared status from the YAML layer onto the domain enum.
fn compile_terminal_status(status: TerminalStatusSchema) -> TerminalStatus {
    match status {
        TerminalStatusSchema::Completed => TerminalStatus::Completed,
        TerminalStatusSchema::Failed => TerminalStatus::Failed,
        TerminalStatusSchema::Cancelled => TerminalStatus::Cancelled,
    }
}

fn compile_state(
    id: String,
    state: StateSchema,
    terminal: bool,
    terminal_status: Option<TerminalStatus>,
    default_timeout_secs: Option<u64>,
    default_retry_policy: Option<&RetryPolicy>,
    primary_repository: Option<&str>,
) -> State {
    let timeout_secs = state.timeout.map(|d| d.as_secs()).or(default_timeout_secs);

    let retry_policy = state
        .retry_policy
        .as_ref()
        .map(compile_retry_policy)
        .or_else(|| default_retry_policy.cloned());

    // A `v1` step's `workspace_access` becomes a section on the primary
    // repository; a `v2` step declares its own.
    let workspace = match (&state.workspace, &state.action, primary_repository) {
        (Some(section), _, _) => Some(compile_step_workspace(section)),
        (None, Some(ActionSchema::Agent(agent)), Some(primary)) => {
            agent.workspace_access.map(|access| match access {
                WorkspaceAccess::ReadOnly => StepWorkspace {
                    root: primary.to_string(),
                    read: vec![primary.to_string()],
                    ..StepWorkspace::default()
                },
                WorkspaceAccess::ReadWrite => StepWorkspace {
                    root: primary.to_string(),
                    ..StepWorkspace::default()
                },
            })
        }
        _ => None,
    };

    // A `v1` agent on a workflow with no repository has no section to carry
    // its access, so the action keeps it.
    let keeps_v1_access = workspace.is_none();

    let (choices, transitions) = compile_exits(state.choices, state.exit, state.transitions);

    State {
        id,
        max_entries: state.max_entries,
        description: state.description,
        action: state
            .action
            .map(|action| compile_action(action, keeps_v1_access)),
        variables: state.variables.into_iter().map(compile_variable).collect(),
        artifacts: state
            .artifacts
            .into_iter()
            .map(|artifact| ArtifactDefinition {
                name: artifact.name,
                description: artifact.description,
            })
            .collect(),
        choices,
        transitions,
        decision: state.decision.map(compile_decision),
        timeout_secs,
        retry_policy,
        terminal,
        terminal_status,
        allow_manual_override: state.allow_manual_override,
        workspace,
        image: state.image,
    }
}

fn compile_step_workspace(section: &StepWorkspaceSchema) -> StepWorkspace {
    StepWorkspace {
        root: section.root.clone(),
        write: section.write.clone(),
        read: section.read.clone(),
    }
}

fn compile_variable(variable: VariableSchema) -> VariableDefinition {
    VariableDefinition {
        name: variable.name,
        description: variable.description,
        scope: variable.scope,
        source: variable.source,
        variable_type: variable.variable_type,
        options: variable.options,
    }
}

/// The choices and transitions of a state. A linear step's `exit` becomes its
/// only choice, under [`EXIT_LABEL`], with its `next` as the one transition.
fn compile_exits(
    choices: Vec<ChoiceSchema>,
    exit: Option<ExitSchema>,
    mut transitions: HashMap<String, String>,
) -> (Vec<ChoiceDefinition>, HashMap<String, String>) {
    let mut compiled: Vec<ChoiceDefinition> = choices.into_iter().map(compile_choice).collect();
    if let Some(exit) = exit {
        transitions.insert(EXIT_LABEL.to_string(), exit.next);
        compiled.push(compile_choice(exit.choice));
    }
    (compiled, transitions)
}

fn compile_choice(choice: ChoiceSchema) -> ChoiceDefinition {
    ChoiceDefinition {
        label: choice.label,
        requires_variables: choice.requires_variables,
        requires_artifacts: choice.requires_artifacts,
        // A blank description says nothing; treating it as absent keeps an
        // empty dash out of every place labels are listed.
        description: non_blank(choice.description),
        requires_human_approval: choice.requires_human_approval,
        resume_comment_field: non_blank(choice.resume_comment_field),
        set: choice.set,
    }
}

fn non_blank(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn compile_action(action: ActionSchema, keep_v1_access: bool) -> ActionConfig {
    match action {
        ActionSchema::Agent(agent) => ActionConfig::Agent(AgentActionConfig {
            harness: agent.harness,
            permission_grants: Vec::new(),
            model: agent.model,
            workspace_access: if keep_v1_access {
                agent.workspace_access
            } else {
                None
            },
            prompt: agent.prompt,
            // The YAML contract has no per-action workspace (§6 lists it as a
            // future extension), so this stays unset.
            workspace: None,
            parameters: agent.parameters,
            read_only_paths: agent.read_only_paths,
            history: agent.history,
        }),
        ActionSchema::Script(script) => ActionConfig::Script(ScriptActionConfig {
            command: script.command,
            args: script.args,
            working_dir: script.working_dir,
            env: script.env,
            exit_codes: script.exit_codes,
            timeout_secs: script.timeout.map(|d| d.as_secs()),
        }),
        ActionSchema::Human(human) => ActionConfig::Human(HumanActionConfig {
            // Name drift: YAML says `message`, the domain says `instruction`.
            instruction: human.message,
            inputs: human
                .inputs
                .into_iter()
                .map(|field| HumanInputField {
                    name: field.name,
                    field_type: match field.field_type {
                        HumanInputTypeSchema::Text => HumanInputType::Text,
                        HumanInputTypeSchema::Select => HumanInputType::Select,
                    },
                    options: field.options,
                    required: field.required,
                    description: field.description,
                })
                .collect(),
            input_schema: None,
        }),
        ActionSchema::Decision(decision) => ActionConfig::Decision(DecisionActionConfig {
            by: decision.by,
            message: decision.message,
        }),
        ActionSchema::Wait(wait) => ActionConfig::Wait(WaitActionConfig {
            // Name drift: YAML says `event_type`, the domain says `event`.
            event: wait.event_type,
            filter: wait.filter,
            timeout_secs: wait.timeout.map(|d| d.as_secs()),
        }),
    }
}

fn compile_decision(decision: DecisionSchema) -> DecisionConfig {
    match decision {
        DecisionSchema::Rules => DecisionConfig::Rules,
        DecisionSchema::Agent(agent) => DecisionConfig::Agent(AgentDecisionConfig {
            harness: agent.harness,
            prompt: agent.prompt,
        }),
        DecisionSchema::Human(human) => DecisionConfig::Human(HumanDecisionConfig {
            message: human.message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RepositorySource, SchemaVersion, WorkspaceAccess, WorkspaceSpec};

    const STATES: &str = r#"
states:
  work:
    action: { type: script, command: ./a.sh }
    choices: [success]
    transitions: { success: done }
  done:
    description: end
terminal_states: [done]
"#;

    fn load(header: &str) -> Result<WorkflowDefinition, LoadError> {
        load_from_str(&format!("{header}{STATES}"))
    }

    const V2_REPOSITORIES: &str = r#"
schema: v2
name: multi
version: 3
globals:
  workspaces:
    code: { primary: true, base_branch: main, repository: "{{ inputs.repo }}", preserve_paths: [Cargo.lock] }
    docs: { base_branch: trunk, path: ../docs }
inputs:
  - { name: repo, type: repository }
"#;

    #[test]
    fn a_v1_git_workspace_converts_to_one_primary_repository() {
        let definition = load(
            "name: legacy\nversion: 2\nglobals:\n  workspace:\n    type: git\n    repository: my-repo\n    ref: develop\n    preserve_paths: [Cargo.lock]\n",
        )
        .unwrap();

        assert_eq!(definition.source_schema, SchemaVersion::V1);
        assert_eq!(definition.version, 2);
        assert_eq!(
            definition.repositories,
            vec![RepositoryDefinition {
                name: "my-repo".to_string(),
                primary: true,
                base_branch: Some("develop".to_string()),
                source: RepositorySource::Git {
                    repository: "my-repo".to_string()
                },
                preserve_paths: vec!["Cargo.lock".to_string()],
            }]
        );
        assert!(matches!(
            definition.primary_workspace_spec(),
            Some(WorkspaceSpec::Git { git_ref: Some(ref r), .. }) if r == "develop"
        ));
    }

    #[test]
    fn a_v1_local_workspace_converts_and_keeps_preserve_paths() {
        let definition = load(
            "name: legacy\nversion: 1\nglobals:\n  workspace:\n    type: local\n    path: ../apps/site/\n    preserve_paths: [\".env.example\"]\n",
        )
        .unwrap();

        let [repository] = definition.repositories.as_slice() else {
            panic!("expected one repository");
        };
        assert_eq!(repository.name, "site");
        assert!(repository.primary);
        assert_eq!(repository.base_branch, None);
        assert_eq!(
            repository.source,
            RepositorySource::Local {
                path: "../apps/site/".to_string()
            }
        );
        assert_eq!(repository.preserve_paths, vec![".env.example".to_string()]);
    }

    #[test]
    fn a_templated_v1_repository_gets_a_fixed_folder_name() {
        let definition = load(
            "name: legacy\nversion: 1\ninputs:\n  - { name: repo, type: repository }\nglobals:\n  workspace:\n    type: git\n    repository: \"{{ inputs.repo }}\"\n",
        )
        .unwrap();

        assert_eq!(definition.repositories[0].name, "repository");
    }

    #[test]
    fn a_missing_schema_is_v1_and_a_v1_without_workspace_has_no_repositories() {
        let definition = load("name: bare\nversion: 1\n").unwrap();
        assert_eq!(definition.source_schema, SchemaVersion::V1);
        assert!(definition.repositories.is_empty());

        let explicit = load("schema: v1\nname: bare\nversion: 1\n").unwrap();
        assert_eq!(explicit.source_schema, SchemaVersion::V1);
    }

    #[test]
    fn v1_workspace_access_converts_to_a_step_workspace_section() {
        let yaml = r#"
name: legacy
version: 1
globals:
  workspace: { type: git, repository: repo-a }
states:
  review:
    action: { type: agent, prompt: look, workspace_access: read-only }
    choices: [done]
    transitions: { done: done }
  done:
    description: end
terminal_states: [done]
"#;
        let definition = load_from_str(yaml).unwrap();
        let review = &definition.states["review"];
        assert_eq!(
            review.workspace,
            Some(StepWorkspace {
                root: "repo-a".to_string(),
                write: vec![],
                read: vec!["repo-a".to_string()],
            })
        );
        assert_eq!(review.workspace_access(), Some(WorkspaceAccess::ReadOnly));
    }

    #[test]
    fn v1_read_only_access_survives_without_a_declared_repository() {
        let yaml = r#"
name: legacy
version: 1
states:
  review:
    action: { type: agent, prompt: look, workspace_access: read-only }
    choices: [done]
    transitions: { done: done }
  done:
    description: end
terminal_states: [done]
"#;
        let definition = load_from_str(yaml).unwrap();
        let Some(ActionConfig::Agent(agent)) = &definition.states["review"].action else {
            panic!("an agent action");
        };
        assert_eq!(agent.workspace_access, Some(WorkspaceAccess::ReadOnly));
    }

    #[test]
    fn a_v2_file_loads_repositories_and_step_workspaces() {
        let yaml = format!(
            "{V2_REPOSITORIES}{}",
            r#"
states:
  work:
    workspaces: { root: ".", write: [docs], read: [code] }
    action: { type: agent, prompt: write the docs }
    choices: [success]
    transitions: { success: done }
  done:
    description: end
terminal_states: [done]
"#
        );
        let definition = load_from_str(&yaml).unwrap();

        assert_eq!(definition.source_schema, SchemaVersion::V2);
        assert_eq!(definition.version, 3);
        assert_eq!(definition.repositories.len(), 2);
        assert_eq!(definition.primary_repository().unwrap().name, "code");
        assert_eq!(
            definition.repositories[0].source,
            RepositorySource::Git {
                repository: "{{ inputs.repo }}".to_string()
            }
        );
        assert_eq!(
            definition.repositories[1].base_branch.as_deref(),
            Some("trunk")
        );
        assert_eq!(
            definition.states["work"].workspace,
            Some(StepWorkspace {
                root: ".".to_string(),
                write: vec!["docs".to_string()],
                read: vec!["code".to_string()],
            })
        );
        assert_eq!(
            definition.states["work"].workspace_access(),
            Some(WorkspaceAccess::ReadWrite)
        );
    }

    #[test]
    fn v2_rejects_v1_constructs() {
        let with_workspace = load(
            "schema: v2\nname: x\nversion: 1\nglobals:\n  workspace: { type: git, repository: r }\n",
        )
        .unwrap_err();
        assert!(
            with_workspace.to_string().contains("globals.workspace"),
            "{with_workspace}"
        );

        let yaml = r#"
schema: v2
name: x
version: 1
states:
  a:
    action: { type: agent, prompt: p, workspace_access: read-only }
    choices: [done]
    transitions: { done: done }
  done:
    description: end
terminal_states: [done]
"#;
        let error = load_from_str(yaml).unwrap_err();
        assert!(error.to_string().contains("workspace_access"), "{error}");
    }

    #[test]
    fn v1_rejects_v2_constructs() {
        let error = load(
            "name: x\nversion: 1\nglobals:\n  workspaces:\n    a: { primary: true, repository: r, base_branch: main }\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("schema: v2"), "{error}");
    }

    #[test]
    fn an_unknown_schema_or_field_is_an_error() {
        assert!(load("schema: v3\nname: x\nversion: 1\n").is_err());
        assert!(load("schema: v2\nname: x\nversion: 1\nsurprise: true\n").is_err());
        let error = load(
            "schema: v2\nname: x\nversion: 1\nglobals:\n  workspaces:\n    a: { primary: true, repository: r, extra: 1 }\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("extra"), "{error}");
    }

    #[test]
    fn a_workspace_name_used_twice_is_an_error() {
        let error = load(
            "schema: v2\nname: x\nversion: 1\nglobals:\n  workspaces:\n    a: { primary: true, path: p }\n    a: { path: q }\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate"), "{error}");
    }

    #[test]
    fn a_repository_names_exactly_one_source() {
        for source in ["", ", repository: r, path: p"] {
            let error = load(&format!(
                "schema: v2\nname: x\nversion: 1\nglobals:\n  workspaces:\n    a: {{ primary: true{source} }}\n"
            ))
            .unwrap_err();
            assert!(error.to_string().contains("exactly one"), "{error}");
        }
    }
}
