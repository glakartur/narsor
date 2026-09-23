//! Durable storage of executions: `state.json`, `context.json`, artifacts.

use super::{PersistenceError, Result};
use crate::domain::{Context, ExecutionId, WorkflowExecution};
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, ensure_dir, read_json, write_json_atomic,
};
use crate::persistence::event_log::EventLog;
use crate::persistence::paths::{Paths, DEFAULT_ROOT};
use std::fs;
use std::path::{Path, PathBuf};

/// Filesystem store for workflow executions.
///
/// A concrete struct rather than a trait: the engine only ever writes to the
/// filesystem, and there is nothing to substitute — the tests run against real
/// temporary directories, because the whole value of this layer lies in real
/// file operations. Introducing a trait now would add a dispatch boundary that
/// no second implementation justifies; a later unit that needs one can extract
/// it from these signatures without changing any call site.
///
/// `state.json` is the authority for an execution, including its embedded
/// `context`. `context.json` is written alongside it as a standalone
/// projection: it is what the CLI and the TUI read to show the current
/// variables, and it is the file the engine rewrites on the D10 path, where
/// only the context changed.
#[derive(Debug, Clone)]
pub struct ExecutionStore {
    paths: Paths,
}

impl ExecutionStore {
    /// Creates a store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            paths: Paths::new(root),
        }
    }

    /// Creates a store at the default root (`./.orchestrator`).
    pub fn with_default_root() -> Self {
        Self::new(DEFAULT_ROOT)
    }

    /// The path layout this store uses.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Persists an execution: `state.json` plus the `context.json` projection.
    ///
    /// The directory structure is created on demand, so no caller ever has to
    /// `mkdir` first.
    pub fn save_execution(&self, execution: &WorkflowExecution) -> Result<()> {
        self.create_execution_dirs(&execution.id)?;
        write_json_atomic(&self.paths.context_file(&execution.id), &execution.context)?;
        write_json_atomic(&self.paths.state_file(&execution.id), execution)
    }

    /// Loads an execution from its `state.json`.
    pub fn load_execution(&self, execution_id: &str) -> Result<WorkflowExecution> {
        let path = self.paths.state_file(execution_id);
        if !path.exists() {
            return Err(PersistenceError::ExecutionNotFound {
                execution_id: execution_id.to_string(),
                path,
            });
        }
        read_json(&path)
    }

    /// Persists only `context.json`.
    ///
    /// The engine is the sole caller (D10): an action never writes the context
    /// itself.
    pub fn save_context(&self, execution_id: &str, context: &Context) -> Result<()> {
        write_json_atomic(&self.paths.context_file(execution_id), context)
    }

    /// Loads `context.json`.
    pub fn load_context(&self, execution_id: &str) -> Result<Context> {
        let path = self.paths.context_file(execution_id);
        if !path.exists() {
            return Err(PersistenceError::ExecutionNotFound {
                execution_id: execution_id.to_string(),
                path,
            });
        }
        read_json(&path)
    }

    /// True when an execution with this identifier is stored.
    pub fn exists(&self, execution_id: &str) -> bool {
        self.paths.state_file(execution_id).exists()
    }

    /// Lists the identifiers of all stored executions, sorted.
    ///
    /// A root that does not exist yet simply holds no executions.
    pub fn list_execution_ids(&self) -> Result<Vec<ExecutionId>> {
        let dir = self.paths.executions_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(PersistenceError::io(&dir, source)),
        };

        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PersistenceError::io(&dir, source))?;
            if !entry.path().is_dir() {
                continue;
            }
            if let Some(id) = entry.file_name().to_str() {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// The event log of one execution.
    pub fn event_log(&self, execution_id: &str) -> EventLog {
        EventLog::new(self.paths.events_dir(execution_id))
    }

    /// Creates and returns the directory for one artifact of an execution.
    pub fn artifact_dir(&self, execution_id: &ExecutionId, artifact_id: &str) -> Result<PathBuf> {
        let dir = self.paths.artifact_dir(execution_id, artifact_id);
        ensure_dir(&dir)?;
        Ok(dir)
    }

    /// Creates the full directory structure of one execution.
    pub fn create_execution_dirs(&self, execution_id: &str) -> Result<()> {
        ensure_dir(&self.paths.execution_dir(execution_id))?;
        ensure_dir(&self.paths.events_dir(execution_id))?;
        ensure_dir(&self.paths.artifacts_dir(execution_id))
    }

    /// Removes every orphaned `.tmp` file below the root.
    ///
    /// Call this once at startup: such a file is the trace of a crash between a
    /// write and its rename, never data to recover. It is deliberately explicit
    /// rather than implicit in every read, so that a read never mutates the
    /// store and a diagnosing operator can inspect the leftovers first.
    ///
    /// Returns the paths that were removed.
    pub fn cleanup_orphaned_temp_files(&self) -> Result<Vec<PathBuf>> {
        let mut removed = Vec::new();
        for execution_id in self.list_execution_ids()? {
            removed.extend(cleanup_orphaned_temp_files(
                &self.paths.execution_dir(&execution_id),
            )?);
            removed.extend(cleanup_orphaned_temp_files(
                &self.paths.events_dir(&execution_id),
            )?);
            removed.extend(cleanup_orphaned_temp_files(
                &self.paths.artifacts_dir(&execution_id),
            )?);
        }
        Ok(removed)
    }

    /// The directory holding one execution.
    pub fn execution_dir(&self, execution_id: &str) -> PathBuf {
        self.paths.execution_dir(execution_id)
    }

    /// The persistence root.
    pub fn root(&self) -> &Path {
        self.paths.root()
    }
}

impl Default for ExecutionStore {
    fn default() -> Self {
        Self::with_default_root()
    }
}
