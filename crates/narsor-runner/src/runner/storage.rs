//! The layout of one task storage and the runner's own bookkeeping on it.
//!
//! A task storage is the one directory a backend mounts into a task's
//! workload and keeps across compute recreation (R9). Everything a resumed
//! runner needs lives here, and nothing else does:
//!
//! ```text
//! {task storage}/
//!     state/        Engine state root (`ExecutionStore`): the one execution,
//!                   its event log, conversations, and the copied launch spec
//!     workspace/    the clone of the pinned commit (R39), or an empty folder
//!     home/         the task's private, persistent `$HOME` (R17, R42)
//!     materials/    owner materials delivered by the control plane (U9),
//!                   one verified, complete directory per material id
//!     outbox/       documents the task leaves for its owner: each folder
//!                   is published as a material once no engine pass runs
//!     narsor/       the runner's bookkeeping, below
//!         runner.lock        held for as long as one runner works here
//!         initialized.json   the initialization marker (R39)
//!         session.json       the incarnation that worked here last, and
//!                            whether it stopped cleanly (R13)
//!         report.json        the last snapshot sequence handed out
//!         inputs.json        owner inputs this storage already consumed
//!         commands.json      operator commands this storage already carried out
//!         harness.json       the installation's default harness this storage
//!                            last switched to (U7); its launch spec's harness
//!                            is the fallback until there is one
//!         conductor.json     a per-task conductor harness an older version
//!                            kept; neither read nor removed any more
//!         work/              scratch of the conductor's isolated invocations
//!         claude-profile/    the Claude profile source as the last start
//!                            saw it, the base of the next merge (KTD6)
//!         workspace.partial/ a clone under way, never a workspace
//!         materials.partial/ a material delivery under way
//! ```
//!
//! `materials/` and `outbox/` sit beside the workspace, never inside it: a
//! material reaches the agent without ever touching the product
//! repository's history or status (R30, AE7).
//!
//! Secrets are deliberately *not* part of this layout: they are mounted
//! read-only outside it and only ever referenced from it (see
//! [`super::secrets`]).
//!
//! # One runner at a time
//!
//! The Engine's file store assumes a single writer. A backend should never
//! run two workloads on one storage, but "should" is not a guarantee, so the
//! runner takes an exclusive, non-blocking `flock(2)` on `runner.lock` before
//! it touches anything else and keeps it until it exits. The kernel drops it
//! when the process dies, so a lost workload never strands its storage.

use std::collections::BTreeSet;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::persistence::{read_json, write_json_atomic, ExecutionStore};

use super::RunnerError;

/// Directory of the Engine state root.
pub const STATE_DIR: &str = "state";
/// Directory of the work tree.
pub const WORKSPACE_DIR: &str = "workspace";
/// Directory of the private HOME.
pub const HOME_DIR: &str = "home";
/// Directory of the runner's own bookkeeping.
pub const BOOKKEEPING_DIR: &str = "narsor";
/// Directory of delivered materials (U9).
pub const MATERIALS_DIR: &str = "materials";
/// Directory the task leaves documents in for its owner (U9).
pub const OUTBOX_DIR: &str = "outbox";

/// Entries a filesystem may put into a fresh volume on its own. Anything
/// else in an uninitialized storage means it is not empty (R39).
const FILESYSTEM_ENTRIES: [&str; 1] = ["lost+found"];

/// Schema version of the bookkeeping records.
const RECORD_VERSION: u32 = 1;

/// The paths of one task storage.
#[derive(Debug, Clone)]
pub struct TaskStorage {
    root: PathBuf,
}

impl TaskStorage {
    /// The storage mounted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The mount point.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `{root}/state`
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    /// `{root}/workspace`
    pub fn workspace_dir(&self) -> PathBuf {
        self.root.join(WORKSPACE_DIR)
    }

    /// `{root}/home`
    pub fn home_dir(&self) -> PathBuf {
        self.root.join(HOME_DIR)
    }

    /// `{root}/narsor`
    pub fn bookkeeping_dir(&self) -> PathBuf {
        self.root.join(BOOKKEEPING_DIR)
    }

    /// `{root}/materials`
    pub fn materials_dir(&self) -> PathBuf {
        self.root.join(MATERIALS_DIR)
    }

    /// `{root}/outbox`
    pub fn outbox_dir(&self) -> PathBuf {
        self.root.join(OUTBOX_DIR)
    }

    /// `{root}/narsor/materials.partial`
    pub fn partial_materials_dir(&self) -> PathBuf {
        self.bookkeeping_dir().join("materials.partial")
    }

    /// `{root}/narsor/workspace.partial`
    pub fn partial_clone_dir(&self) -> PathBuf {
        self.bookkeeping_dir().join("workspace.partial")
    }

    fn lock_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("runner.lock")
    }

    fn marker_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("initialized.json")
    }

    fn session_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("session.json")
    }

    fn report_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("report.json")
    }

    fn inputs_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("inputs.json")
    }

    fn commands_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("commands.json")
    }

    fn harness_file(&self) -> PathBuf {
        self.bookkeeping_dir().join("harness.json")
    }

    /// `{root}/narsor/claude-profile`: the copy of the Claude profile
    /// source taken at the previous start, the base of the next merge.
    pub fn claude_profile_base_dir(&self) -> PathBuf {
        self.bookkeeping_dir().join("claude-profile")
    }

    /// `{root}/narsor/work`: where the conductor keeps its scratch.
    pub fn conductor_work_dir(&self) -> PathBuf {
        self.bookkeeping_dir().join("work")
    }

    /// The Engine store rooted on this storage.
    pub fn store(&self) -> ExecutionStore {
        ExecutionStore::new(self.state_dir())
    }

    /// Takes the storage's exclusive runner lock, refusing at once when
    /// another process holds it.
    pub fn lock(&self) -> Result<StorageLock, RunnerError> {
        create_dir(&self.bookkeeping_dir())?;
        let path = self.lock_file();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| RunnerError::io(&path, source))?;
        self.flock_exclusive(file, &path)
    }

    /// Takes the same exclusive lock for a runner that only reads.
    ///
    /// Nothing is created or opened for writing: the lock file must be there
    /// already, which only a runner that worked on the storage leaves, so a
    /// missing one says the storage was never used
    /// ([`RunnerError::StorageNotInitialized`]). `flock(2)` needs no write
    /// access, so a read-only mount takes the lock too, and the file's
    /// contents and times stay as they were.
    pub fn lock_for_reading(&self) -> Result<StorageLock, RunnerError> {
        let path = self.lock_file();
        let file = fs::File::open(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                RunnerError::StorageNotInitialized(self.root.clone())
            } else {
                RunnerError::io(&path, source)
            }
        })?;
        self.flock_exclusive(file, &path)
    }

    fn flock_exclusive(&self, file: fs::File, path: &Path) -> Result<StorageLock, RunnerError> {
        // SAFETY: `file` owns a valid descriptor for the duration of the call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let source = std::io::Error::last_os_error();
            return Err(if source.kind() == std::io::ErrorKind::WouldBlock {
                RunnerError::StorageBusy(self.root.clone())
            } else {
                RunnerError::io(path, source)
            });
        }
        Ok(StorageLock { _file: file })
    }

    /// Creates the directories every run needs, all on the storage.
    pub fn prepare(&self) -> Result<(), RunnerError> {
        for dir in [
            self.state_dir(),
            self.home_dir(),
            self.bookkeeping_dir(),
            self.materials_dir(),
            self.outbox_dir(),
        ] {
            create_dir(&dir)?;
        }
        Ok(())
    }

    /// Entries of an uninitialized storage that are not this runner's own
    /// (and not the filesystem's), i.e. what makes it not empty.
    pub fn foreign_entries(&self) -> Result<Vec<String>, RunnerError> {
        let ours: BTreeSet<&str> = [
            STATE_DIR,
            WORKSPACE_DIR,
            HOME_DIR,
            BOOKKEEPING_DIR,
            MATERIALS_DIR,
            OUTBOX_DIR,
        ]
        .into_iter()
        .chain(FILESYSTEM_ENTRIES)
        .collect();
        let entries =
            fs::read_dir(&self.root).map_err(|source| RunnerError::io(&self.root, source))?;
        let mut foreign = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RunnerError::io(&self.root, source))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !ours.contains(name.as_str()) {
                foreign.push(name);
            }
        }
        foreign.sort();
        Ok(foreign)
    }

    /// The initialization marker, when the storage was initialized.
    pub fn marker(&self) -> Result<Option<InitializationMarker>, RunnerError> {
        read_optional(&self.marker_file())
    }

    /// Writes the initialization marker — the last step of initialization.
    pub fn write_marker(&self, marker: &InitializationMarker) -> Result<(), RunnerError> {
        write_json_atomic(&self.marker_file(), marker).map_err(RunnerError::from)
    }

    /// The last session record, when a runner ever worked here.
    pub fn session(&self) -> Result<Option<SessionRecord>, RunnerError> {
        read_optional(&self.session_file())
    }

    /// Replaces the session record.
    pub fn write_session(&self, session: &SessionRecord) -> Result<(), RunnerError> {
        write_json_atomic(&self.session_file(), session).map_err(RunnerError::from)
    }

    /// Hands out the next snapshot sequence.
    ///
    /// Persisted before it is used, so sequences keep growing across
    /// incarnations too: the control plane only requires that within one,
    /// but a sequence that never repeats on a storage is simpler to reason
    /// about in a log.
    pub fn next_snapshot_sequence(&self) -> Result<u64, RunnerError> {
        let mut state: ReportState = read_optional(&self.report_file())?.unwrap_or_default();
        state.version = RECORD_VERSION;
        state.last_sequence += 1;
        write_json_atomic(&self.report_file(), &state)?;
        Ok(state.last_sequence)
    }

    /// Records the event cursor of a snapshot the control plane accepted.
    pub fn record_reported_cursor(&self, event_cursor: u64) -> Result<(), RunnerError> {
        let mut state: ReportState = read_optional(&self.report_file())?.unwrap_or_default();
        state.version = RECORD_VERSION;
        state.last_reported_event_cursor = state.last_reported_event_cursor.max(event_cursor);
        write_json_atomic(&self.report_file(), &state).map_err(RunnerError::from)
    }

    /// The highest event cursor the control plane accepted from this storage.
    pub fn reported_cursor(&self) -> Result<u64, RunnerError> {
        Ok(read_optional::<ReportState>(&self.report_file())?
            .map(|state| state.last_reported_event_cursor)
            .unwrap_or(0))
    }

    /// Whether an owner input was already consumed here.
    pub fn input_consumed(&self, input_id: &str) -> Result<bool, RunnerError> {
        Ok(read_optional::<ConsumedInputs>(&self.inputs_file())?
            .is_some_and(|inputs| inputs.consumed.contains(input_id)))
    }

    /// Records an owner input as consumed, before the control plane hears
    /// of it: a redelivery after a lost acknowledgement is then answered
    /// from here instead of reaching the Engine again.
    pub fn record_consumed_input(&self, input_id: &str) -> Result<(), RunnerError> {
        let mut inputs: ConsumedInputs = read_optional(&self.inputs_file())?.unwrap_or_default();
        inputs.version = RECORD_VERSION;
        inputs.consumed.insert(input_id.to_string());
        write_json_atomic(&self.inputs_file(), &inputs).map_err(RunnerError::from)
    }

    /// The default harness this storage last switched to (U7), when it
    /// ever switched: a resumed runner starts on it, not on its launch
    /// spec's.
    pub fn applied_harness(&self) -> Result<Option<String>, RunnerError> {
        Ok(read_optional::<AppliedHarness>(&self.harness_file())?.map(|applied| applied.harness))
    }

    /// Records the default harness the runner switched to.
    pub fn record_applied_harness(&self, harness: &str) -> Result<(), RunnerError> {
        let applied = AppliedHarness {
            version: RECORD_VERSION,
            harness: harness.to_string(),
        };
        write_json_atomic(&self.harness_file(), &applied).map_err(RunnerError::from)
    }

    /// How far an operator command got here (U3).
    pub fn command_progress(&self, command_id: &str) -> Result<CommandProgress, RunnerError> {
        let commands: Option<CarriedOutCommands> = read_optional(&self.commands_file())?;
        Ok(
            match commands.and_then(|commands| commands.progress.get(command_id).cloned()) {
                None => CommandProgress::Unseen,
                Some(None) => CommandProgress::Begun,
                Some(Some(verdict)) => CommandProgress::Done(verdict),
            },
        )
    }

    /// Records that an operator command is about to be carried out, before
    /// the Engine is touched: a runner that dies halfway finds the mark and
    /// refuses to carry the command out a second time.
    pub fn begin_command(&self, command_id: &str) -> Result<(), RunnerError> {
        self.write_command_progress(command_id, None)
    }

    /// Records the verdict on an operator command, before the control plane
    /// hears of it: a redelivery after a lost acknowledgement is then
    /// answered from here without reaching the Engine again.
    pub fn finish_command(
        &self,
        command_id: &str,
        verdict: &CommandVerdict,
    ) -> Result<(), RunnerError> {
        self.write_command_progress(command_id, Some(verdict.clone()))
    }

    fn write_command_progress(
        &self,
        command_id: &str,
        verdict: Option<CommandVerdict>,
    ) -> Result<(), RunnerError> {
        let mut commands: CarriedOutCommands =
            read_optional(&self.commands_file())?.unwrap_or_default();
        commands.version = RECORD_VERSION;
        commands.progress.insert(command_id.to_string(), verdict);
        write_json_atomic(&self.commands_file(), &commands).map_err(RunnerError::from)
    }
}

/// How far an operator command got on a storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandProgress {
    /// Never seen here.
    Unseen,
    /// Started, with no verdict: whoever started it did not survive.
    Begun,
    /// Carried out, or refused, with this verdict.
    Done(CommandVerdict),
}

/// The runner's verdict on an operator command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandVerdict {
    /// Whether the command was carried out.
    pub applied: bool,
    /// Why it was not.
    pub detail: Option<String>,
}

impl CommandVerdict {
    /// The command was carried out.
    pub fn applied() -> Self {
        Self {
            applied: true,
            detail: None,
        }
    }

    /// The command was refused, for this reason.
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            applied: false,
            detail: Some(reason.into()),
        }
    }
}

/// The exclusive runner lock of a storage, released on drop (and by the
/// kernel when the process dies).
#[derive(Debug)]
pub struct StorageLock {
    _file: fs::File,
}

/// What a finished initialization wrote: the storage belongs to this task
/// and this launch for good (R39).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitializationMarker {
    /// Schema version.
    pub version: u32,
    /// The task the storage belongs to.
    pub task_id: String,
    /// Digest of the launch spec it was initialized from.
    pub launch_digest: String,
    /// The commit cloned into the workspace, when there is one.
    pub workspace_commit: Option<String>,
    /// When initialization finished.
    pub initialized_at: DateTime<Utc>,
}

impl InitializationMarker {
    /// A marker for a storage initialized just now.
    pub fn new(task_id: &str, launch_digest: &str, workspace_commit: Option<String>) -> Self {
        Self {
            version: RECORD_VERSION,
            task_id: task_id.to_string(),
            launch_digest: launch_digest.to_string(),
            workspace_commit,
            initialized_at: Utc::now(),
        }
    }
}

/// Which incarnation worked on the storage last, and how it stopped.
///
/// Written when an incarnation starts working and again when it stops at a
/// safe point. A record that was never closed is the storage's own
/// testimony of an unexpected stop: the workload died, or was killed, while
/// work may have been in flight (R13).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRecord {
    /// Schema version.
    pub version: u32,
    /// The incarnation.
    pub incarnation: String,
    /// The generation it was launched at, once the control plane said so.
    /// `None` when it never reached the control plane.
    pub launch_generation: Option<u64>,
    /// When it started working.
    pub started_at: DateTime<Utc>,
    /// How it stopped, when it stopped cleanly.
    pub closed: Option<SessionClose>,
}

impl SessionRecord {
    /// An open session of `incarnation`.
    pub fn open(incarnation: &str, launch_generation: Option<u64>) -> Self {
        Self {
            version: RECORD_VERSION,
            incarnation: incarnation.to_string(),
            launch_generation,
            started_at: Utc::now(),
            closed: None,
        }
    }
}

/// A clean stop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionClose {
    /// When.
    pub at: DateTime<Utc>,
    /// Why, for a person reading the file.
    pub reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReportState {
    version: u32,
    last_sequence: u64,
    last_reported_event_cursor: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct AppliedHarness {
    version: u32,
    harness: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConsumedInputs {
    version: u32,
    consumed: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CarriedOutCommands {
    version: u32,
    /// Command id to its verdict; `None` while it is being carried out.
    progress: std::collections::BTreeMap<String, Option<CommandVerdict>>,
}

fn create_dir(path: &Path) -> Result<(), RunnerError> {
    fs::create_dir_all(path).map_err(|source| RunnerError::io(path, source))
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, RunnerError> {
    if !path.exists() {
        return Ok(None);
    }
    read_json(path).map(Some).map_err(RunnerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("narsor-storage-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_read_lock_on_an_unused_storage_creates_nothing() {
        let root = temp("unused");
        let storage = TaskStorage::new(&root);
        let error = storage.lock_for_reading().unwrap_err();
        assert!(matches!(error, RunnerError::StorageNotInitialized(_)));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_read_lock_excludes_a_working_runner_and_the_other_way_round() {
        let root = temp("exclusion");
        let storage = TaskStorage::new(&root);
        let working = storage.lock().unwrap();
        assert!(matches!(
            storage.lock_for_reading().unwrap_err(),
            RunnerError::StorageBusy(_)
        ));
        drop(working);
        let reading = storage.lock_for_reading().unwrap();
        assert!(matches!(
            storage.lock().unwrap_err(),
            RunnerError::StorageBusy(_)
        ));
        drop(reading);
        assert!(storage.lock().is_ok(), "the lock is let go on drop");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_read_lock_leaves_the_lock_file_as_it_was_and_needs_no_write_access() {
        let root = temp("untouched");
        let storage = TaskStorage::new(&root);
        drop(storage.lock().unwrap());
        let lock_file = storage.bookkeeping_dir().join("runner.lock");
        let before = fs::metadata(&lock_file).unwrap();

        // As on a volume mounted read-only: nothing here may be written.
        fs::set_permissions(&lock_file, fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(storage.bookkeeping_dir(), fs::Permissions::from_mode(0o555)).unwrap();
        let held = storage.lock_for_reading();
        let after = fs::metadata(&lock_file).unwrap();
        fs::set_permissions(storage.bookkeeping_dir(), fs::Permissions::from_mode(0o755)).unwrap();

        assert!(held.is_ok(), "{held:?}");
        assert_eq!(after.len(), before.len());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        drop(held);
        let _ = fs::remove_dir_all(root);
    }
}
