//! Reads `{catalog}/automations/*/` and `{catalog}/queues/*.yaml`.
//!
//! ```text
//! automations/{name}/automation.yaml
//!     description: Adds and removes a merge request label
//!     params: [mr_url]                    # names the scripts read
//!     secrets:                            # names only, never values (R5)
//!       - {name: gitlab-token, env: GITLAB_TOKEN}
//! automations/{name}/check.sh             # required (R3)
//! automations/{name}/in.sh, out.sh        # optional
//!
//! queues/{name}.yaml
//!     automations: [gitlab-mr-label]
//!     mode: on_episodes                   # or always
//!     schedule: {cron: "*/5 * * * *"}     # or {interval_secs: 300}
//!     condition: {}                       # optional, handed to the scripts
//!     tasks:                              # optional, `always` queues only
//!       workflow_id: triage               # must be in the workflow catalogue
//!       mapping: {summary: title}
//! ```
//!
//! The directory or file name is the identifier, as in the environment
//! catalogue. A load never fails as a whole: each file that cannot be used
//! is one [`CatalogProblem`] and the rest still load (R7, AE8). A queue that
//! names a missing or invalid automation is a problem and is not active.
//!
//! Nothing is cached. The loader records which scripts exist; the script
//! text is read with [`AutomationCatalog::script`] when a run is claimed,
//! so a change applies to the next run (R2).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use narsor_protocol::controller::WorkspaceRequest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::observer_name::{name_like_problem, param_name_problem, queue_name_problem};
use crate::domain::WorkflowDefinition;
use crate::environment::catalog::EXTENSIONS;
use crate::observers::store::script_digest;
use crate::observers::{
    CreateTaskEffect, ObserverMode, ScheduleConfig, SecretRef, DEFAULT_TASK_LIMIT_PER_RUN,
    MAX_CONDITION_BYTES, MAX_SCRIPT_BYTES, MAX_SECRET_REFS, MAX_TASK_LIMIT_PER_RUN,
};
use crate::persistence::CatalogPaths;

/// The definition file inside an automation directory.
pub const AUTOMATION_FILE: &str = "automation.yaml";

/// The script phases of an automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    /// Runs when a gate episode enters the queue (optional).
    In,
    /// Runs on the queue schedule (required).
    Check,
    /// Runs when a gate episode leaves the queue (optional).
    Out,
}

impl HookPhase {
    /// Every phase, in run order.
    pub const ALL: [HookPhase; 3] = [Self::In, Self::Check, Self::Out];

    /// The wire name (`in`, `check`, `out`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Check => "check",
            Self::Out => "out",
        }
    }

    /// The script file of the phase inside an automation directory.
    pub fn script_file(self) -> &'static str {
        match self {
            Self::In => "in.sh",
            Self::Check => "check.sh",
            Self::Out => "out.sh",
        }
    }
}

impl fmt::Display for HookPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One valid automation of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Automation {
    /// The directory name.
    pub name: String,
    /// What it does, for the views.
    pub description: String,
    /// The params it reads from a queue entry.
    pub params: Vec<String>,
    /// The secrets the worker hands to its scripts, by name.
    pub secrets: Vec<SecretRef>,
    /// The phases whose script exists; `check` is always there.
    pub scripts: BTreeSet<HookPhase>,
    /// `sha256:` digest of `automation.yaml` as it was read: a stopped pair
    /// runs again when it changes.
    pub definition_digest: String,
}

impl Automation {
    /// Whether the automation has a script for `phase`.
    pub fn has(&self, phase: HookPhase) -> bool {
        self.scripts.contains(&phase)
    }
}

/// One active queue of the catalogue: every automation it names is valid.
#[derive(Debug, Clone, PartialEq)]
pub struct Queue {
    /// The file stem.
    pub name: String,
    /// The automations that serve it, in file order.
    pub automations: Vec<String>,
    /// Whether its automations run for entries only or on every tick.
    pub mode: ObserverMode,
    /// When its automations run.
    pub schedule: ScheduleConfig,
    /// Parameters handed to the scripts as they are (a JSON object).
    pub condition: serde_json::Value,
    /// The tasks its scripts may create; only an `always` queue has them.
    pub tasks: Option<QueueTasks>,
    /// `sha256:` digest of the queue file as it was read: a stopped pair
    /// runs again when it changes.
    pub definition_digest: String,
}

/// The task-creation settings of an `always` queue.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueTasks {
    /// What every created task runs, checked against the workflow
    /// catalogue at load.
    pub effect: CreateTaskEffect,
    /// Most tasks one run may create.
    pub task_limit_per_run: u32,
    /// Whether the first run imports what already matches the condition.
    pub import_backlog: bool,
}

/// The fingerprint of one (queue, automation) pair's definition: the queue
/// file, the automation's `automation.yaml` and its `check` script, each by
/// its digest. A stopped pair runs again when it changes. An empty
/// `check_digest` stands for a script that could not be read.
pub fn pair_definition_digest(
    queue: &Queue,
    automation: &Automation,
    check_digest: &str,
) -> String {
    script_digest(
        format!(
            "{}\n{}\n{check_digest}",
            queue.definition_digest, automation.definition_digest
        )
        .as_bytes(),
    )
}

/// One catalogue file that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogProblem {
    /// The file or directory, relative to the catalogue root.
    pub file: PathBuf,
    /// Why it is not used.
    pub reason: String,
}

impl fmt::Display for CatalogProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.reason)
    }
}

/// What one load of the catalogue found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutomationCatalogLoad {
    /// The active queues, by name.
    pub queues: BTreeMap<String, Queue>,
    /// The valid automations, by name.
    pub automations: BTreeMap<String, Automation>,
    /// Every file that was not used, in path order.
    pub problems: Vec<CatalogProblem>,
}

impl AutomationCatalogLoad {
    /// The `on_episodes` queue `name`, whose automations can answer a gate;
    /// otherwise why none can (R9, R27): the queue runs in mode `always`,
    /// its file cannot be used, or it is not in the catalogue.
    pub fn gate_queue(&self, name: &str) -> Result<&Queue, String> {
        match self.queues.get(name) {
            Some(queue) if queue.mode == ObserverMode::OnEpisodes => Ok(queue),
            Some(_) => Err(format!(
                "queue '{name}' runs its automations in mode always, which cannot answer a \
                 gate; the gate waits for a person"
            )),
            None if self
                .problems
                .iter()
                .any(|problem| problem_names_queue(&problem.file, name)) =>
            {
                Err(format!(
                    "the catalogue file of queue '{name}' cannot be used; the gate waits for a \
                     person"
                ))
            }
            None => Err(format!(
                "queue '{name}' is not in the catalogue; the gate waits for a person"
            )),
        }
    }

    /// The problem that makes the pair (`queue`, `automation`) unusable
    /// now, when one does: the queue (loaded or not) may still name the
    /// automation, which is not loaded and a file of it has a problem; or
    /// the queue is not loaded and a file of it has a problem. The pair may be
    /// back once the file is fixed, so what it has not run yet waits rather
    /// than counting as removed. A problem that leaves the pair usable (a
    /// second file of a loaded queue) holds nothing back, and a loaded
    /// queue that no longer names the automation removed it for good.
    pub fn pair_problem(&self, queue: &str, automation: &str) -> Option<&CatalogProblem> {
        let named =
            |test: &dyn Fn(&Path) -> bool| self.problems.iter().find(|problem| test(&problem.file));
        let loaded_queue = self.queues.get(queue);
        let still_named = loaded_queue
            .is_none_or(|loaded| loaded.automations.iter().any(|name| name == automation));
        // The automation's own file first: a broken automation also makes
        // every queue that names it unusable.
        if still_named && !self.automations.contains_key(automation) {
            if let Some(problem) = named(&|file| problem_names_automation(file, automation)) {
                return Some(problem);
            }
        }
        if loaded_queue.is_none() {
            return named(&|file| problem_names_queue(file, queue));
        }
        None
    }

    /// The automations of `queue` that have a `hook` script, in the queue
    /// file's order.
    pub fn automations_with(&self, queue: &Queue, hook: HookPhase) -> Vec<String> {
        queue
            .automations
            .iter()
            .filter(|name| {
                self.automations
                    .get(*name)
                    .is_some_and(|automation| automation.has(hook))
            })
            .cloned()
            .collect()
    }
}

/// Whether the catalogue file `file` (relative to the catalogue root) is the
/// file of queue `queue`.
pub(crate) fn problem_names_queue(file: &Path, queue: &str) -> bool {
    file.parent() == Some(Path::new("queues"))
        && file.file_stem().and_then(|stem| stem.to_str()) == Some(queue)
}

/// Whether the catalogue file or directory `file` (relative to the
/// catalogue root) belongs to automation `automation`.
fn problem_names_automation(file: &Path, automation: &str) -> bool {
    file.starts_with(Path::new("automations").join(automation))
}

/// Why a script could not be read.
#[derive(Debug, Error)]
pub enum AutomationCatalogError {
    /// This control plane reads no catalogue.
    #[error("this server has no automation catalogue")]
    NoCatalogue,
    /// The automation has no script for the phase (or does not exist).
    #[error("automation '{automation}' has no {phase} script")]
    MissingScript {
        /// The automation.
        automation: String,
        /// The phase.
        phase: HookPhase,
    },
    /// The script is there but cannot be used.
    #[error("script '{path}' cannot be used: {reason}")]
    Unusable {
        /// The script file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

/// A script's text as read now, and its `sha256:` digest for the run history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationScript {
    /// The script text.
    pub text: String,
    /// `sha256:` digest of the text.
    pub digest: String,
}

/// The automations and queues under one catalogue root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationCatalog {
    paths: CatalogPaths,
}

// ---------------------------------------------------------------------------
// File formats
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationFile {
    description: String,
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    secrets: Vec<SecretFile>,
}

/// `name` and `env` default to empty so that an entry carrying a `value`
/// is refused for its value, not for a missing name.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    env: String,
    #[serde(default)]
    value: Option<serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueFile {
    automations: Vec<String>,
    mode: ObserverMode,
    schedule: ScheduleFile,
    #[serde(default)]
    condition: Option<serde_json::Value>,
    #[serde(default)]
    tasks: Option<TasksFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleFile {
    #[serde(default)]
    interval_secs: Option<u64>,
    #[serde(default)]
    cron: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TasksFile {
    workflow_id: String,
    #[serde(default)]
    workflow_version: Option<u32>,
    #[serde(default)]
    workspaces: BTreeMap<String, WorkspaceRequest>,
    #[serde(default)]
    backend_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    mapping: BTreeMap<String, String>,
    #[serde(default = "default_task_limit_per_run")]
    task_limit_per_run: u32,
    #[serde(default)]
    import_backlog: bool,
}

fn default_task_limit_per_run() -> u32 {
    DEFAULT_TASK_LIMIT_PER_RUN
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl AutomationCatalog {
    /// The catalogue under `root`. Nothing is read here.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            paths: CatalogPaths::new(root),
        }
    }

    /// Reads every automation and queue now. `workflow` finds the valid
    /// catalogue definition of a workflow id, or says why there is none; an
    /// `always` queue that creates tasks is checked against it.
    pub fn load(
        &self,
        workflow: impl Fn(&str) -> Result<WorkflowDefinition, String>,
    ) -> AutomationCatalogLoad {
        let mut load = AutomationCatalogLoad::default();
        let mut invalid = BTreeSet::new();
        for (name, path) in self.list(&self.paths.automations_dir(), &mut load.problems) {
            match self.read_automation(&name, &path) {
                Ok(automation) => {
                    load.automations.insert(name, automation);
                }
                Err(problem) => {
                    invalid.insert(name);
                    load.problems.push(problem);
                }
            }
        }
        for (name, path) in self.queue_files(&mut load.problems) {
            match self.read_queue(&name, &path, &load.automations, &invalid, &workflow) {
                Ok(queue) => {
                    load.queues.insert(name, queue);
                }
                Err(reason) => load.problems.push(self.problem(&path, reason)),
            }
        }
        load.problems.sort_by(|a, b| a.file.cmp(&b.file));
        load
    }

    /// The text of `automation`'s `phase` script as it is on disk now.
    pub fn script(
        &self,
        automation: &str,
        phase: HookPhase,
    ) -> Result<AutomationScript, AutomationCatalogError> {
        let missing = || AutomationCatalogError::MissingScript {
            automation: automation.to_string(),
            phase,
        };
        if name_like_problem(automation, true).is_some() {
            return Err(missing());
        }
        let path = self
            .paths
            .automations_dir()
            .join(automation)
            .join(phase.script_file());
        let unusable = |reason: String| AutomationCatalogError::Unusable {
            path: path.clone(),
            reason,
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Err(missing()),
            Err(error) => return Err(unusable(error.to_string())),
        };
        if bytes.len() > MAX_SCRIPT_BYTES {
            return Err(unusable(format!(
                "it has more than {MAX_SCRIPT_BYTES} bytes"
            )));
        }
        let digest = script_digest(&bytes);
        let text = String::from_utf8(bytes).map_err(|_| unusable("it is not UTF-8".into()))?;
        Ok(AutomationScript { text, digest })
    }

    fn read_automation(&self, name: &str, dir: &Path) -> Result<Automation, CatalogProblem> {
        if !dir.is_dir() {
            return Err(self.problem(
                dir,
                "not a directory: an automation is a directory with automation.yaml and check.sh",
            ));
        }
        if let Some(reason) = name_like_problem(name, true) {
            return Err(self.problem(dir, format!("invalid automation name: {reason}")));
        }
        let file = dir.join(AUTOMATION_FILE);
        let (parsed, definition_digest): (AutomationFile, String) =
            match std::fs::read_to_string(&file) {
                Ok(text) => (
                    serde_yaml::from_str(&text).map_err(|error| self.problem(&file, error))?,
                    script_digest(text.as_bytes()),
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    return Err(self.problem(&file, "missing: every automation needs one"))
                }
                Err(error) => return Err(self.problem(&file, error)),
            };
        let params =
            validate_params(parsed.params).map_err(|reason| self.problem(&file, reason))?;
        let secrets =
            validate_secrets(parsed.secrets).map_err(|reason| self.problem(&file, reason))?;
        let mut scripts = BTreeSet::new();
        for phase in HookPhase::ALL {
            let script = dir.join(phase.script_file());
            match std::fs::metadata(&script) {
                Ok(meta) if !meta.is_file() => {
                    return Err(self.problem(&script, "not a regular file"))
                }
                Ok(meta) if meta.len() > MAX_SCRIPT_BYTES as u64 => {
                    return Err(self.problem(
                        &script,
                        format!("the script has more than {MAX_SCRIPT_BYTES} bytes"),
                    ))
                }
                Ok(_) => {
                    scripts.insert(phase);
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if phase == HookPhase::Check {
                        return Err(
                            self.problem(&script, "missing: every automation needs a check script")
                        );
                    }
                }
                Err(error) => return Err(self.problem(&script, error)),
            }
        }
        Ok(Automation {
            name: name.to_string(),
            description: parsed.description,
            params,
            secrets,
            scripts,
            definition_digest,
        })
    }

    fn read_queue(
        &self,
        name: &str,
        file: &Path,
        automations: &BTreeMap<String, Automation>,
        invalid: &BTreeSet<String>,
        workflow: &impl Fn(&str) -> Result<WorkflowDefinition, String>,
    ) -> Result<Queue, String> {
        if let Some(reason) = queue_name_problem(name) {
            return Err(format!("invalid queue name: {reason}"));
        }
        let text = std::fs::read_to_string(file).map_err(|error| error.to_string())?;
        let parsed: QueueFile = serde_yaml::from_str(&text).map_err(|error| error.to_string())?;
        if parsed.automations.is_empty() {
            return Err("'automations' must name at least one automation".into());
        }
        let mut seen = BTreeSet::new();
        for automation in &parsed.automations {
            if !seen.insert(automation.as_str()) {
                return Err(format!("automation '{automation}' is listed twice"));
            }
            if invalid.contains(automation) {
                return Err(format!(
                    "automation '{automation}' is invalid (see its own problem)"
                ));
            }
            if !automations.contains_key(automation) {
                return Err(format!("automation '{automation}' is not in the catalogue"));
            }
        }
        let schedule =
            ScheduleConfig::from_parts(parsed.schedule.interval_secs, parsed.schedule.cron)
                .map_err(|error| error.to_string())?;
        let condition = validate_condition(parsed.condition)?;
        let tasks = match (parsed.tasks, parsed.mode) {
            (None, _) => None,
            (Some(_), ObserverMode::OnEpisodes) => {
                return Err(
                    "'tasks' needs 'mode: always': an on_episodes queue creates no tasks".into(),
                )
            }
            (Some(tasks), ObserverMode::Always) => Some(validate_tasks(tasks, workflow)?),
        };
        Ok(Queue {
            name: name.to_string(),
            automations: parsed.automations,
            mode: parsed.mode,
            schedule,
            condition,
            tasks,
            definition_digest: script_digest(text.as_bytes()),
        })
    }

    /// The queue files by name: `.yaml` before `.yml`; any other file is
    /// not a queue and is ignored, as the environment catalogue does.
    fn queue_files(&self, problems: &mut Vec<CatalogProblem>) -> Vec<(String, PathBuf)> {
        let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut listed: Vec<(usize, String, PathBuf)> = self
            .list(&self.paths.queues_dir(), problems)
            .into_iter()
            .filter_map(|(file_name, path)| {
                let (stem, extension) = file_name.rsplit_once('.')?;
                let rank = EXTENSIONS.iter().position(|ext| *ext == extension)?;
                Some((rank, stem.to_string(), path))
            })
            .collect();
        listed.sort();
        for (_, stem, path) in listed {
            match files.entry(stem) {
                std::collections::btree_map::Entry::Occupied(taken) => {
                    problems.push(self.problem(
                        &path,
                        format!("another file already defines queue '{}'", taken.key()),
                    ));
                }
                std::collections::btree_map::Entry::Vacant(free) => {
                    free.insert(path);
                }
            }
        }
        files.into_iter().collect()
    }

    /// The entries of `dir` by name, without hidden ones (a mounted
    /// ConfigMap holds `..data` links). An absent directory is empty.
    fn list(&self, dir: &Path, problems: &mut Vec<CatalogProblem>) -> Vec<(String, PathBuf)> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                problems.push(self.problem(dir, error));
                return Vec::new();
            }
        };
        let mut listed = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    problems.push(self.problem(dir, error));
                    continue;
                }
            };
            let path = entry.path();
            match entry.file_name().into_string() {
                Ok(name) if name.starts_with('.') => {}
                Ok(name) => listed.push((name, path)),
                Err(_) => problems.push(self.problem(&path, "the name is not UTF-8")),
            }
        }
        listed.sort();
        listed
    }

    fn problem(&self, path: &Path, reason: impl ToString) -> CatalogProblem {
        CatalogProblem {
            file: path
                .strip_prefix(self.paths.root())
                .unwrap_or(path)
                .to_path_buf(),
            reason: reason.to_string(),
        }
    }
}

/// Param names follow the rule of a gate's `watch.params`: not blank, no
/// whitespace; and each is listed once.
fn validate_params(params: Vec<String>) -> Result<Vec<String>, String> {
    let mut seen = BTreeSet::new();
    for param in &params {
        if let Some(problem) = param_name_problem(param) {
            return Err(problem);
        }
        if !seen.insert(param.as_str()) {
            return Err(format!("param '{param}' is listed twice"));
        }
    }
    Ok(params)
}

fn validate_secrets(secrets: Vec<SecretFile>) -> Result<Vec<SecretRef>, String> {
    if secrets.len() > MAX_SECRET_REFS {
        return Err(format!("at most {MAX_SECRET_REFS} secrets"));
    }
    let mut envs = BTreeSet::new();
    let mut refs = Vec::with_capacity(secrets.len());
    for secret in secrets {
        if secret.value.is_some() {
            return Err(format!(
                "secret '{}' carries a value: a catalogue file names a secret, the worker \
                 reads its value from the secrets directory",
                secret.name
            ));
        }
        let secret = SecretRef {
            name: secret.name,
            env: secret.env,
        };
        secret.validate().map_err(|error| error.to_string())?;
        if !envs.insert(secret.env.clone()) {
            return Err(format!("secret env '{}' is used twice", secret.env));
        }
        refs.push(secret);
    }
    Ok(refs)
}

fn validate_condition(condition: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let condition = condition.unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if !condition.is_object() {
        return Err("'condition' must be a map".into());
    }
    let len = serde_json::to_vec(&condition)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if len > MAX_CONDITION_BYTES {
        return Err(format!(
            "'condition' has more than {MAX_CONDITION_BYTES} bytes"
        ));
    }
    Ok(condition)
}

/// The task-creation settings, checked as `observer create` checked them:
/// the workflow must be in the catalogue, its version is the catalogue's
/// unless the file pins one, and every mapped input must be declared.
fn validate_tasks(
    tasks: TasksFile,
    workflow: &impl Fn(&str) -> Result<WorkflowDefinition, String>,
) -> Result<QueueTasks, String> {
    if tasks.task_limit_per_run == 0 || tasks.task_limit_per_run > MAX_TASK_LIMIT_PER_RUN {
        return Err(format!(
            "'tasks.task_limit_per_run' must be between 1 and {MAX_TASK_LIMIT_PER_RUN}"
        ));
    }
    let definition = workflow(&tasks.workflow_id)
        .map_err(|reason| format!("workflow '{}' cannot be used: {reason}", tasks.workflow_id))?;
    let effect = CreateTaskEffect {
        workflow_version: tasks.workflow_version.unwrap_or(definition.version),
        workflow_id: tasks.workflow_id,
        workspaces: tasks.workspaces,
        backend_id: tasks.backend_id,
        profile: tasks.profile,
        mapping: tasks.mapping,
    };
    effect.validate().map_err(|error| error.to_string())?;
    effect
        .validate_against(&definition)
        .map_err(|error| error.to_string())?;
    Ok(QueueTasks {
        effect,
        task_limit_per_run: tasks.task_limit_per_run,
        import_backlog: tasks.import_backlog,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalogue root in the temp directory, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-automations-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).expect("catalogue root");
            Self(root)
        }

        fn write(&self, relative: &str, text: &str) -> &Self {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
            std::fs::write(path, text).expect("write");
            self
        }

        /// An automation with a definition and the given scripts.
        fn automation(&self, name: &str, scripts: &[&str]) -> &Self {
            self.write(
                &format!("automations/{name}/automation.yaml"),
                "description: test\nparams: [mr_url]\nsecrets:\n  - {name: gitlab-token, env: GITLAB_TOKEN}\n",
            );
            for script in scripts {
                self.write(
                    &format!("automations/{name}/{script}"),
                    "#!/bin/sh\nexit 0\n",
                );
            }
            self
        }

        fn queue(&self, name: &str, automation: &str) -> &Self {
            self.write(
                &format!("queues/{name}.yaml"),
                &format!(
                    "automations: [{automation}]\nmode: on_episodes\nschedule: {{cron: \"*/5 * * * *\"}}\n"
                ),
            )
        }

        fn load(&self) -> AutomationCatalogLoad {
            AutomationCatalog::new(&self.0)
                .load(|id| Err(format!("workflow '{id}' is not in this test catalogue")))
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn problem_files(load: &AutomationCatalogLoad) -> Vec<String> {
        load.problems
            .iter()
            .map(|problem| problem.file.display().to_string())
            .collect()
    }

    #[test]
    fn loads_a_queue_served_by_an_automation_with_all_three_scripts() {
        let root = Root::new();
        root.automation("gitlab-mr-label", &["in.sh", "check.sh", "out.sh"])
            .queue("mr-review", "gitlab-mr-label");

        let load = root.load();

        assert_eq!(load.problems, Vec::new());
        let queue = &load.queues["mr-review"];
        assert_eq!(queue.automations, vec!["gitlab-mr-label".to_string()]);
        assert_eq!(queue.mode, ObserverMode::OnEpisodes);
        assert_eq!(
            queue.schedule.cron().map(|cron| cron.to_string()),
            Some("*/5 * * * *".into())
        );
        assert!(queue.tasks.is_none());
        let automation = &load.automations["gitlab-mr-label"];
        assert!(HookPhase::ALL.iter().all(|phase| automation.has(*phase)));
        assert_eq!(automation.params, vec!["mr_url".to_string()]);
        assert_eq!(automation.secrets[0].env, "GITLAB_TOKEN");
    }

    #[test]
    fn a_queue_naming_a_missing_automation_is_not_active_and_others_still_load() {
        let root = Root::new();
        root.automation("gitlab-mr-label", &["check.sh"])
            .queue("mr-review", "gitlab-mr-label")
            .queue("x", "missing-automation");

        let load = root.load();

        assert_eq!(load.queues.keys().collect::<Vec<_>>(), vec!["mr-review"]);
        assert_eq!(problem_files(&load), vec!["queues/x.yaml"]);
        assert!(load.problems[0].reason.contains("missing-automation"));
    }

    /// A case name, what it writes beside a valid queue, the problem file
    /// and a part of the reason.
    type BadFileCase = (&'static str, fn(&Root), &'static str, &'static str);

    #[test]
    fn each_bad_file_is_one_problem_and_the_rest_loads() {
        let cases: [BadFileCase; 6] = [
            (
                "automation without check.sh",
                |root| {
                    root.automation("broken", &["in.sh"]);
                },
                "automations/broken/check.sh",
                "check script",
            ),
            (
                "cron with a seconds field",
                |root| {
                    root.write(
                        "queues/bad.yaml",
                        "automations: [ok]\nmode: on_episodes\nschedule: {cron: \"0 */5 * * * *\"}\n",
                    );
                },
                "queues/bad.yaml",
                "seconds field",
            ),
            (
                "YAML parse error",
                |root| {
                    root.write("queues/bad.yaml", "automations: [ok\nmode: :\n");
                },
                "queues/bad.yaml",
                "",
            ),
            (
                "secret carrying a value",
                |root| {
                    root.write(
                        "automations/leaky/automation.yaml",
                        "description: x\nsecrets:\n  - {name: token, env: TOKEN, value: hunter2}\n",
                    )
                    .write("automations/leaky/check.sh", "exit 0\n");
                },
                "automations/leaky/automation.yaml",
                "carries a value",
            ),
            (
                "always queue whose workflow is not in the catalogue",
                |root| {
                    root.write(
                        "queues/bad.yaml",
                        "automations: [ok]\nmode: always\nschedule: {interval_secs: 300}\ntasks: {workflow_id: nope}\n",
                    );
                },
                "queues/bad.yaml",
                "workflow 'nope'",
            ),
            (
                "unknown field",
                |root| {
                    root.write(
                        "queues/bad.yaml",
                        "automations: [ok]\nmode: always\nschedule: {interval_secs: 300}\npaused: true\n",
                    );
                },
                "queues/bad.yaml",
                "unknown field",
            ),
        ];
        for (case, write_bad, file, reason) in cases {
            let root = Root::new();
            root.automation("ok", &["check.sh"]).queue("good", "ok");
            write_bad(&root);

            let load = root.load();

            assert_eq!(problem_files(&load), vec![file], "{case}");
            assert!(
                load.problems[0].reason.contains(reason),
                "{case}: {}",
                load.problems[0]
            );
            assert!(load.queues.contains_key("good"), "{case}");
            assert!(!load.queues.contains_key("bad"), "{case}");
        }
    }

    #[test]
    fn an_absent_catalogue_has_no_queues_and_no_problems() {
        let root = Root::new();

        assert_eq!(root.load(), AutomationCatalogLoad::default());
    }

    #[test]
    fn a_script_is_read_as_it_is_now_and_a_missing_phase_is_refused() {
        let root = Root::new();
        root.automation("gitlab-mr-label", &["check.sh"]);
        let catalog = AutomationCatalog::new(&root.0);
        let first = catalog
            .script("gitlab-mr-label", HookPhase::Check)
            .expect("check");

        root.write(
            "automations/gitlab-mr-label/check.sh",
            "#!/bin/sh\nexit 1\n",
        );
        let second = catalog
            .script("gitlab-mr-label", HookPhase::Check)
            .expect("check");

        assert_eq!(second.text, "#!/bin/sh\nexit 1\n");
        assert_ne!(first.digest, second.digest);
        assert!(matches!(
            catalog.script("gitlab-mr-label", HookPhase::In),
            Err(AutomationCatalogError::MissingScript { .. })
        ));
    }
}
