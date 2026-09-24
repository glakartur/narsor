//! Where a workflow definition comes from.
//!
//! `narsor run compound-engineering` has to work from an installed
//! binary, and `cargo install --path .` installs the binary and nothing else.
//! So the two shipped definitions are **embedded** with `include_str!`
//! ([`BUILTIN_WORKFLOWS`]): they travel with the binary that runs them and can
//! never drift from it.
//!
//! # Precedence
//!
//! [`WorkflowResolver::resolve`] turns what the operator typed into a
//! [`ResolvedWorkflow`], trying, in this order and stopping at the first hit:
//!
//! 1. **a file that exists** at that exact path, relative or absolute — the
//!    behaviour the CLI has always had, and the one that must keep winning so
//!    that `narsor run ./draft.yaml` never resolves to something else;
//! 2. `{root}/workflows/<name>.yaml` (then `.yml`) — the checkout's own copies,
//!    and a directory `workflow eject --to` can target;
//! 3. the user directory — `$XDG_CONFIG_HOME/narsor/workflows/<name>.yaml`,
//!    or `$HOME/.config/narsor/workflows/<name>.yaml` when
//!    `XDG_CONFIG_HOME` is unset;
//! 4. the **built-in** of that name.
//!
//! A local copy therefore overrides a built-in, which is the point: eject the
//! built-in, edit it, and every later `run` picks up the edited one. The name
//! may be written with or without a `.yaml`/`.yml` suffix.
//!
//! # Nothing here reads the environment twice
//!
//! The user directory is resolved from the environment **once**, when the
//! resolver is built ([`WorkflowResolver::new`]), and can be set explicitly
//! ([`WorkflowResolver::with_user_dir`]). That is what lets the tests point the
//! user directory at a temporary directory without mutating the process
//! environment — and what guarantees no test can write into a developer's real
//! `~/.config`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::{ValidationError, WorkflowDefinition};
use crate::workflow::{load_from_str, validate_source, LoadError};

/// Directory, under `--root` and under the user config directory, that holds
/// workflow definitions.
pub const WORKFLOWS_DIR: &str = "workflows";

/// Directory of this application inside the user's config directory.
pub const USER_CONFIG_DIR: &str = "narsor";

/// The file extensions a workflow definition may use, in lookup order.
pub const EXTENSIONS: [&str; 2] = ["yaml", "yml"];

/// A workflow definition compiled into the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinWorkflow {
    /// The name it is reached by, without an extension.
    pub name: &'static str,
    /// The YAML, byte for byte as it is in `workflows/` in the repository.
    pub source: &'static str,
}

/// Every workflow shipped inside the binary.
///
/// `include_str!` rather than a file read: an installed `narsor` has no
/// checkout to read from, and an embedded copy cannot fall out of step with
/// the binary. `tests/builtin_workflows.rs` asserts these strings are
/// byte-identical to the files under `workflows/`.
pub const BUILTIN_WORKFLOWS: &[BuiltinWorkflow] = &[
    BuiltinWorkflow {
        name: "compound-engineering",
        source: include_str!("../../workflows/compound-engineering.yaml"),
    },
    BuiltinWorkflow {
        name: "open-mercato-sdlc",
        source: include_str!("../../workflows/open-mercato-sdlc.yaml"),
    },
];

/// The built-in of that name, if there is one.
pub fn builtin(name: &str) -> Option<&'static BuiltinWorkflow> {
    let name = workflow_name(name);
    BUILTIN_WORKFLOWS
        .iter()
        .find(|builtin| builtin.name == name)
}

/// The names of the built-ins, for an error message or a listing.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN_WORKFLOWS.iter().map(|entry| entry.name).collect()
}

/// Strips a `.yaml`/`.yml` suffix, so `foo`, `foo.yaml` and `foo.yml` are one
/// name.
pub fn workflow_name(reference: &str) -> &str {
    for extension in EXTENSIONS {
        if let Some(stem) = reference.strip_suffix(&format!(".{extension}")) {
            return stem;
        }
    }
    reference
}

/// Where a resolved definition was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowOrigin {
    /// An existing file the operator pointed at.
    File(PathBuf),
    /// `{root}/workflows/<name>.yaml`.
    Root(PathBuf),
    /// The user's own directory.
    User(PathBuf),
    /// Compiled into the binary.
    BuiltIn,
}

impl WorkflowOrigin {
    /// The word printed in a listing or after `workflow show`.
    pub fn label(&self) -> &'static str {
        match self {
            WorkflowOrigin::File(_) => "file",
            WorkflowOrigin::Root(_) => "root",
            WorkflowOrigin::User(_) => "user",
            WorkflowOrigin::BuiltIn => "built-in",
        }
    }

    /// The file it was read from — `None` for a built-in, which has no path.
    pub fn path(&self) -> Option<&Path> {
        match self {
            WorkflowOrigin::File(path)
            | WorkflowOrigin::Root(path)
            | WorkflowOrigin::User(path) => Some(path),
            WorkflowOrigin::BuiltIn => None,
        }
    }
}

impl fmt::Display for WorkflowOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.path() {
            Some(path) => write!(formatter, "{} ({})", self.label(), path.display()),
            None => write!(formatter, "{}", self.label()),
        }
    }
}

/// A definition that was found, together with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWorkflow {
    /// The canonical name, without an extension.
    pub name: String,
    /// Where it was found.
    pub origin: WorkflowOrigin,
    /// The YAML itself.
    pub source: String,
}

impl ResolvedWorkflow {
    /// The file it was read from, if it has one.
    pub fn path(&self) -> Option<&Path> {
        self.origin.path()
    }

    /// Compiles it into the domain model.
    pub fn load(&self) -> Result<WorkflowDefinition, LoadError> {
        load_from_str(&self.source)
    }

    /// Every static violation it has, all at once.
    pub fn validate(&self) -> Result<Vec<ValidationError>, LoadError> {
        validate_source(&self.source)
    }
}

/// One entry of the listing behind `narsor workflow list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowEntry {
    /// The name it is reached by.
    pub name: String,
    /// Where the copy that *wins* lives.
    pub origin: WorkflowOrigin,
    /// The definition's `description`, when the file declares a readable one.
    pub description: Option<String>,
    /// The copies this one shadows, in the order they would have been tried.
    ///
    /// Never silently dropped: an operator has to be able to see that the
    /// workflow they are about to run is their own copy and not the shipped
    /// one.
    pub shadows: Vec<WorkflowOrigin>,
}

impl WorkflowEntry {
    /// Whether this entry hides another copy of the same name.
    pub fn is_override(&self) -> bool {
        !self.shadows.is_empty()
    }

    /// The sources this entry hides, as `workflow list` prints them.
    pub fn shadow_labels(&self) -> String {
        self.shadows
            .iter()
            .map(|origin| origin.label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Everything that can stop a reference from resolving.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// Nothing matched — the message says where it looked.
    #[error(
        "No workflow matches '{reference}'.\n  Looked for:\n{}\n  Available built-in workflows: {}.",
        searched.iter().map(|place| format!("    - {place}")).collect::<Vec<_>>().join("\n"),
        builtins.join(", ")
    )]
    NotFound {
        /// What the operator typed.
        reference: String,
        /// Every place that was tried, in order.
        searched: Vec<String>,
        /// The names that are always available.
        builtins: Vec<String>,
    },

    /// A candidate file was there but could not be read.
    #[error("Could not read workflow file '{path}': {source}")]
    Read {
        /// The file that failed.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

/// A directory that is searched, and the origin a hit in it carries.
type SearchDir = (PathBuf, fn(PathBuf) -> WorkflowOrigin);

/// Resolves a workflow reference against the search path.
#[derive(Debug, Clone)]
pub struct WorkflowResolver {
    root: PathBuf,
    user_dir: Option<PathBuf>,
}

impl WorkflowResolver {
    /// A resolver for one data directory, with the user directory taken from
    /// the environment.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            user_dir: user_workflows_dir(),
        }
    }

    /// The same, with the user directory named explicitly.
    ///
    /// `None` disables that step entirely, which is how a test guarantees it
    /// never reads — or writes — the developer's own `~/.config`.
    pub fn with_user_dir(root: impl Into<PathBuf>, user_dir: Option<PathBuf>) -> Self {
        Self {
            root: root.into(),
            user_dir,
        }
    }

    /// `{root}/workflows`.
    pub fn root_dir(&self) -> PathBuf {
        self.root.join(WORKFLOWS_DIR)
    }

    /// The user's own workflow directory, when there is one.
    pub fn user_dir(&self) -> Option<&Path> {
        self.user_dir.as_deref()
    }

    /// Resolves what the operator typed: a file path first, then a name.
    pub fn resolve(&self, reference: &str) -> Result<ResolvedWorkflow, ResolveError> {
        let candidate = Path::new(reference);
        if candidate.is_file() {
            let source =
                std::fs::read_to_string(candidate).map_err(|source| ResolveError::Read {
                    path: candidate.to_path_buf(),
                    source,
                })?;
            return Ok(ResolvedWorkflow {
                name: file_name(candidate),
                origin: WorkflowOrigin::File(absolute(candidate)),
                source,
            });
        }

        self.resolve_name_with(reference, Some(reference.to_string()))
    }

    /// Resolves a *name* only: the search directories and then the built-ins.
    ///
    /// This is the half that a stored execution re-resolves with, where a file
    /// that happens to exist in the current working directory must not be able
    /// to take over.
    pub fn resolve_name(&self, name: &str) -> Result<ResolvedWorkflow, ResolveError> {
        self.resolve_name_with(name, None)
    }

    fn resolve_name_with(
        &self,
        reference: &str,
        file_candidate: Option<String>,
    ) -> Result<ResolvedWorkflow, ResolveError> {
        let name = workflow_name(reference).to_string();
        let mut searched = Vec::new();
        if let Some(candidate) = file_candidate {
            searched.push(format!("a file at '{candidate}'"));
        }

        // A name is a name, not a path: `../../etc/passwd` never becomes a
        // directory lookup. It can still be reached as a file, above.
        if !is_plain_name(&name) {
            return Err(self.not_found(reference, searched));
        }

        for (directory, origin) in self.search_dirs() {
            for extension in EXTENSIONS {
                let path = directory.join(format!("{name}.{extension}"));
                searched.push(format!("'{}'", path.display()));
                if path.is_file() {
                    let source =
                        std::fs::read_to_string(&path).map_err(|source| ResolveError::Read {
                            path: path.clone(),
                            source,
                        })?;
                    return Ok(ResolvedWorkflow {
                        name,
                        origin: origin(path),
                        source,
                    });
                }
            }
        }

        searched.push("the built-in workflows".to_string());
        if let Some(builtin) = builtin(&name) {
            return Ok(ResolvedWorkflow {
                name,
                origin: WorkflowOrigin::BuiltIn,
                source: builtin.source.to_string(),
            });
        }

        Err(self.not_found(reference, searched))
    }

    /// Every workflow reachable right now, by name, overrides marked.
    ///
    /// Sorted by name, so two runs of `workflow list` never disagree.
    pub fn list(&self) -> Vec<WorkflowEntry> {
        let mut names: Vec<String> = builtin_names()
            .into_iter()
            .map(ToString::to_string)
            .collect();
        for (directory, _) in self.search_dirs() {
            names.extend(definition_names_in(&directory));
        }
        names.sort();
        names.dedup();

        let mut entries = Vec::new();
        for name in names {
            let mut origins = self.origins_of(&name);
            if origins.is_empty() {
                continue;
            }
            let winner = origins.remove(0);
            let description = self
                .resolve_name(&name)
                .ok()
                .and_then(|resolved| description_of(&resolved.source));
            entries.push(WorkflowEntry {
                name,
                origin: winner,
                description,
                shadows: origins,
            });
        }
        entries
    }

    /// Every copy of one name, in precedence order.
    fn origins_of(&self, name: &str) -> Vec<WorkflowOrigin> {
        let mut origins = Vec::new();
        if !is_plain_name(name) {
            return origins;
        }
        for (directory, origin) in self.search_dirs() {
            for extension in EXTENSIONS {
                let path = directory.join(format!("{name}.{extension}"));
                if path.is_file() {
                    origins.push(origin(path));
                }
            }
        }
        if builtin(name).is_some() {
            origins.push(WorkflowOrigin::BuiltIn);
        }
        origins
    }

    /// The directories searched, in precedence order.
    fn search_dirs(&self) -> Vec<SearchDir> {
        let mut dirs: Vec<SearchDir> = vec![(self.root_dir(), WorkflowOrigin::Root)];
        if let Some(user) = &self.user_dir {
            dirs.push((user.clone(), WorkflowOrigin::User));
        }
        dirs
    }

    /// The "nothing matched" error, with the places it looked.
    fn not_found(&self, reference: &str, searched: Vec<String>) -> ResolveError {
        ResolveError::NotFound {
            reference: reference.to_string(),
            searched,
            builtins: builtin_names()
                .into_iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

/// `$XDG_CONFIG_HOME/narsor`, or `$HOME/.config/narsor`.
///
/// `None` when neither variable is set, which is a normal state in a
/// container: the step that would have used it is skipped rather than guessed
/// at (R9). An empty `XDG_CONFIG_HOME` counts as unset, because that is how
/// an unset key of a ConfigMap renders.
pub fn user_config_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join(USER_CONFIG_DIR))
}

/// `$XDG_CONFIG_HOME/narsor/workflows`, or `$HOME/.config/…`.
pub fn user_workflows_dir() -> Option<PathBuf> {
    Some(user_config_dir()?.join(WORKFLOWS_DIR))
}

/// Whether a reference can be looked up as a bare name in a directory.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}

/// The definition names a directory holds.
fn definition_names_in(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let matches = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| EXTENSIONS.contains(&extension));
        if !matches {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            names.push(stem.to_string());
        }
    }
    names
}

/// The `description` of a definition, without compiling it.
///
/// A file that cannot be parsed still belongs in the listing — with no
/// description rather than not at all.
fn description_of(source: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Head {
        #[serde(default)]
        description: Option<String>,
    }

    serde_yaml::from_str::<Head>(source)
        .ok()
        .and_then(|head| head.description)
        .map(|description| description.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|description| !description.is_empty())
}

/// The canonical name of a file: its stem.
fn file_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The absolute form of a path, when the filesystem can give one.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
