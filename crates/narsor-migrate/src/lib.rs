//! `narsor-migrate`: the one-time move of an installation to the per-binary
//! layout (D82; R10, KTD10 of the controller-protocol plan).
//!
//! The installer of a new version runs it after the old programs stopped;
//! the programs themselves never move files (`narsor-server` only refuses
//! to start on an old layout). One run:
//!
//! 1. finds what is left of the old layout ([`Target::plan`]); nothing, or a
//!    marker saying the move is done, ends the run;
//! 2. prints the plan: what moves where, what is deleted, what the backup
//!    holds and how large it may get;
//! 3. refuses, before it changes anything, while a program holds a lock of
//!    the installation, a runner of the old local controller lives or
//!    Docker cannot tell whether one does ([`refusal`]);
//! 4. asks for confirmation (the caller decides how);
//! 5. writes the backup ([`backup`]), refusing when the space is short;
//! 6. moves the server's files ([`server`]) and the local controller's
//!    ([`controller`]), deletes the stale files and writes the markers, the
//!    server's last: a server marker means the whole installation moved.
//!
//! A repeat finishes what a crash left.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use narsor_controller::local::paths::{ControllerStatePaths, ControllerWorkPaths};
use narsor_shared::config::old_layout::{OldRoots, OLD_SERVER_ENTRIES};
use narsor_shared::config::Resolved;
use narsor_store::paths::ServerStatePaths;

pub mod backup;
pub mod controller;
pub mod mover;
pub mod refusal;
pub mod server;
#[cfg(test)]
mod test_support;

pub use controller::LegacyDirs;
pub use mover::Mover;
pub use refusal::{ContainerProbe, DockerProbe};

/// Exit status: the move is done, there was nothing to move, or a dry run
/// found nothing to refuse.
pub const EXIT_OK: i32 = 0;
/// Exit status: the move started and stopped; the message names the file.
/// A repeat finishes it.
pub const EXIT_FAILED: i32 = 1;
/// Exit status: the command line or the configuration is wrong.
pub const EXIT_USAGE: i32 = 2;
/// Exit status: a check refused before anything changed (a running
/// program, a live runner, Docker without an answer, too little space).
pub const EXIT_REFUSED: i32 = 3;
/// Exit status: the move was not confirmed; nothing changed.
pub const EXIT_NOT_CONFIRMED: i32 = 4;

/// What to move.
#[derive(Debug, Clone)]
pub enum Target {
    /// An installation: the server's files and its local controller's, at
    /// the old roots its configuration resolves.
    Installation(Installation),
    /// A controller started by hand (`narsor-controller local`) on another
    /// machine: its old directories only.
    Controller(ControllerMove),
}

/// The old and new places of an installation.
#[derive(Debug, Clone)]
pub struct Installation {
    /// Where the files were.
    pub old: OldRoots,
    /// The server's new directory.
    pub server: ServerStatePaths,
    /// The local controller's part.
    pub controller: ControllerMove,
    /// `global.root`: the default backup goes below it.
    pub root: PathBuf,
}

/// The old and new places of a local controller's files.
#[derive(Debug, Clone)]
pub struct ControllerMove {
    /// Where its files were.
    pub legacy: LegacyDirs,
    /// Its new state directory.
    pub state: ControllerStatePaths,
    /// Its new work directory.
    pub work: ControllerWorkPaths,
    /// The state root its new state directory is below: the default backup
    /// of a controller started by hand goes below it.
    pub state_root: PathBuf,
}

impl Installation {
    /// The installation `config` resolves (scope of `narsor-server`).
    pub fn of(config: &Resolved) -> Self {
        let old = OldRoots::of(config);
        // The tool reads the roots on purpose: it is the one program that
        // knows both layouts (R6 c).
        let (state_root, work_root) = (config.state_dir(), config.work_dir());
        Self {
            server: ServerStatePaths::new(state_root),
            controller: ControllerMove {
                legacy: LegacyDirs {
                    state: old.local_controller_state(),
                    work: old.local_controller_work(),
                },
                state: ControllerStatePaths::new(state_root),
                work: ControllerWorkPaths::new(work_root),
                state_root: state_root.to_path_buf(),
            },
            old,
            root: config.root().to_path_buf(),
        }
    }
}

impl ControllerMove {
    /// A hand-started controller whose old credential directory was
    /// `legacy_state` and whose old storage directory was `legacy_work`,
    /// to move below `state_root` and `work_root`.
    pub fn by_hand(
        legacy_state: PathBuf,
        legacy_work: PathBuf,
        state_root: &Path,
        work_root: &Path,
    ) -> Self {
        Self {
            legacy: LegacyDirs {
                state: legacy_state,
                work: legacy_work,
            },
            state: ControllerStatePaths::new(state_root),
            work: ControllerWorkPaths::new(work_root),
            state_root: state_root.to_path_buf(),
        }
    }

    /// The old entries that move, each with its new place.
    fn moves(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut moves: Vec<(PathBuf, PathBuf)> = controller::STATE_ENTRIES
            .iter()
            .chain([&controller::CREDENTIAL_LOCK])
            .map(|name| (self.legacy.state.join(name), self.state.dir().join(name)))
            .collect();
        moves.extend(controller::WORK_ENTRIES.iter().map(|(name, to_state)| {
            let to = if *to_state {
                self.state.dir()
            } else {
                self.work.dir()
            };
            (self.legacy.work.join(name), to.join(name))
        }));
        moves
    }

    fn done(&self) -> bool {
        mover::marker_present(self.state.dir())
    }

    fn present(&self) -> bool {
        [&self.legacy.state, &self.legacy.work]
            .iter()
            .any(|dir| dir.symlink_metadata().is_ok())
    }

    fn checks(&self) -> refusal::Checks {
        refusal::Checks {
            locks: vec![
                refusal::Lock {
                    path: self.legacy.state.join(controller::CREDENTIAL_LOCK),
                    holder: "an old narsor-controller",
                },
                refusal::Lock {
                    path: self.state.credential_lock_file(),
                    holder: "a narsor-controller of the new version",
                },
            ],
            workload_dirs: vec![self.legacy.work.clone(), self.work.dir().to_path_buf()],
            sources: self.moves().into_iter().map(|(from, _)| from).collect(),
        }
    }
}

/// What a run would do, computed from what is on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Old entries that exist, each with its new place.
    pub moves: Vec<(PathBuf, PathBuf)>,
    /// Stale files that will be deleted.
    pub deletions: Vec<PathBuf>,
    /// Old entries that stay for the operator to delete.
    pub left: Vec<PathBuf>,
    /// What the backup holds.
    pub backup_sources: Vec<PathBuf>,
}

impl Target {
    /// What is left to move; `None` when nothing is (no old layout, or the
    /// marker says the move is done).
    pub fn plan(&self) -> Option<Plan> {
        let exists = |path: &PathBuf| path.symlink_metadata().is_ok();
        let existing = |moves: Vec<(PathBuf, PathBuf)>| -> Vec<(PathBuf, PathBuf)> {
            moves
                .into_iter()
                .filter(|(from, _)| exists(from) || exists(&aside(from)))
                .collect()
        };
        match self {
            Target::Installation(installation) => {
                if mover::marker_present(installation.server.dir()) {
                    return None;
                }
                let old = &installation.old;
                let controller = &installation.controller;
                let server_moves = OLD_SERVER_ENTRIES
                    .iter()
                    .map(|name| (old.state.join(name), installation.server.dir().join(name)))
                    .collect();
                let mut moves = existing(server_moves);
                if !controller.done() {
                    moves.extend(existing(controller.moves()));
                }
                let deletions: Vec<PathBuf> = server::STALE_FILES
                    .iter()
                    .map(|name| old.state.join(name))
                    .filter(exists)
                    .collect();
                let left: Vec<PathBuf> = server::LEFT_IN_PLACE
                    .iter()
                    .map(|name| old.state.join(name))
                    .filter(exists)
                    .collect();
                if moves.is_empty() && deletions.is_empty() && !controller.present() {
                    return None;
                }
                let mut backup_sources: Vec<PathBuf> = OLD_SERVER_ENTRIES
                    .iter()
                    .flat_map(|name| [old.state.join(name), aside(&old.state.join(name))])
                    .chain(deletions.iter().cloned())
                    .chain(left.iter().cloned())
                    .chain([
                        controller.legacy.state.clone(),
                        controller.legacy.work.clone(),
                    ])
                    .filter(exists)
                    .collect();
                dedup_same_place(&mut backup_sources);
                Some(Plan {
                    moves,
                    deletions,
                    left,
                    backup_sources,
                })
            }
            Target::Controller(controller) => {
                if controller.done() || !controller.present() {
                    return None;
                }
                // Only the entries that move: an old storage directory given
                // as the new work root also holds the new directory.
                let mut backup_sources: Vec<PathBuf> = controller
                    .moves()
                    .into_iter()
                    .flat_map(|(from, _)| [aside(&from), from])
                    .filter(exists)
                    .collect();
                dedup_same_place(&mut backup_sources);
                Some(Plan {
                    moves: existing(controller.moves()),
                    deletions: Vec::new(),
                    left: Vec::new(),
                    backup_sources,
                })
            }
        }
    }

    /// The default backup archive for a run started at `now`:
    /// `<global.root>/backups/` for an installation, `<state root>/backups/`
    /// for a controller started by hand.
    pub fn default_backup(&self, now: chrono::DateTime<chrono::Utc>) -> PathBuf {
        let root = match self {
            Target::Installation(installation) => &installation.root,
            Target::Controller(controller) => &controller.state_root,
        };
        root.join("backups").join(backup::file_name(now))
    }

    fn checks(&self) -> refusal::Checks {
        match self {
            Target::Installation(installation) => {
                let old = &installation.old;
                let mut checks = installation.controller.checks();
                checks.locks.extend([
                    refusal::Lock {
                        path: old.state.join(server::OLD_INSTANCE_LOCK),
                        holder: "an old narsor daemon (stop it with the old `narsor-server stop`)",
                    },
                    refusal::Lock {
                        path: installation.server.daemon_instance_lock_file(),
                        holder: "a narsor-server of the new version",
                    },
                ]);
                checks
                    .sources
                    .extend(OLD_SERVER_ENTRIES.iter().map(|name| old.state.join(name)));
                checks
            }
            Target::Controller(controller) => controller.checks(),
        }
    }
}

/// `<path>.narsor-moved`: a source a crash left renamed aside.
fn aside(path: &Path) -> PathBuf {
    mover::with_suffix(path, mover::MOVED_SUFFIX)
}

/// Drops a later entry that names the same place as an earlier one (the
/// state and work roots are one directory with the defaults).
fn dedup_same_place(paths: &mut Vec<PathBuf>) {
    let mut seen = Vec::new();
    paths.retain(|path| {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

/// Where the backup goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupChoice {
    /// [`Target::default_backup`].
    Default,
    /// A new archive in this directory.
    In(PathBuf),
    /// No backup (`--no-backup`).
    Skip,
}

/// How a run behaves.
#[derive(Debug, Clone)]
pub struct Options {
    /// Print the plan and run the checks only.
    pub dry_run: bool,
    /// Where the backup goes.
    pub backup: BackupChoice,
    /// Skip the question to Docker about old runner containers.
    pub assume_containers_stopped: bool,
    /// How entries move (by rename where possible).
    pub mover: Mover,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            backup: BackupChoice::Default,
            assume_containers_stopped: false,
            mover: Mover::new(),
        }
    }
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No old layout, or the marker says the move is done.
    NothingToMigrate,
    /// A dry run: the plan was printed, nothing changed.
    DryRun(Plan),
    /// The move is done.
    Migrated(Summary),
}

/// What the move did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// The backup archive, when one was written.
    pub backup: Option<backup::Written>,
    /// The old entries that moved.
    pub moved: Vec<String>,
    /// The stale files deleted.
    pub deleted: Vec<PathBuf>,
    /// Claude Code session directories renamed.
    pub sessions: usize,
    /// Stored executions whose workspace paths were rewritten.
    pub executions: usize,
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A check refused; nothing changed.
    Refused(String),
    /// Not confirmed; nothing changed.
    NotConfirmed,
    /// The move started and stopped.
    Failed(String),
}

impl Failure {
    /// The exit status of the command for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Failure::Refused(_) => EXIT_REFUSED,
            Failure::NotConfirmed => EXIT_NOT_CONFIRMED,
            Failure::Failed(_) => EXIT_FAILED,
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(reason) => write!(f, "refused, nothing was changed:\n{reason}"),
            Failure::NotConfirmed => f.write_str("not confirmed, nothing was changed"),
            Failure::Failed(reason) => write!(
                f,
                "the migration stopped: {reason}\nCorrect the cause and run narsor-migrate \
                 again: it finishes what was started."
            ),
        }
    }
}

/// Free space the backup needs beyond its estimate: 10 %.
const SPACE_MARGIN_PERCENT: u64 = 10;

/// One run (see the crate documentation). `out` gets the plan and the
/// summary; `confirm` is asked once, after the checks, before anything
/// changes.
pub fn run(
    target: &Target,
    options: &Options,
    probe: &dyn ContainerProbe,
    confirm: &mut dyn FnMut() -> bool,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let failed = |error: io::Error| Failure::Failed(error.to_string());
    let Some(plan) = target.plan() else {
        writeln!(out, "Nothing to migrate.").map_err(failed)?;
        return Ok(Outcome::NothingToMigrate);
    };
    print_plan(target, &plan, out).map_err(failed)?;

    let backup_path = match &options.backup {
        BackupChoice::Default => Some(target.default_backup(chrono::Utc::now())),
        BackupChoice::In(dir) => Some(dir.join(backup::file_name(chrono::Utc::now()))),
        BackupChoice::Skip => None,
    };
    let mut refusals = Vec::new();
    if let Some(path) = &backup_path {
        let estimate = backup::estimate(&plan.backup_sources);
        let dir = path.parent().unwrap_or(Path::new("/"));
        let free = backup::free_space(dir)
            .map_err(|error| Failure::Refused(format!("{}: {error}", dir.display())))?;
        writeln!(
            out,
            "Backup: {} (up to {}; {} free there)",
            path.display(),
            human(estimate),
            human(free)
        )
        .map_err(failed)?;
        let needed = estimate + estimate * SPACE_MARGIN_PERCENT / 100;
        if free < needed {
            refusals.push(format!(
                "the backup needs up to {} and {} has {} free; pass --backup-dir with a \
                 directory on a larger filesystem",
                human(needed),
                dir.display(),
                human(free)
            ));
        }
    } else {
        writeln!(out, "Backup: none (--no-backup)").map_err(failed)?;
    }
    let foreign = owned_by_someone_else(&plan);
    if !foreign.is_empty() {
        refusals.push(format!(
            "these entries belong to another user; run narsor-migrate as their owner, \
             not as root: {}",
            foreign
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let locks = refusal::check(&target.checks(), probe, options.assume_containers_stopped);
    if let Err(reason) = &locks {
        refusals.push(reason.clone());
    }
    if !refusals.is_empty() {
        return Err(Failure::Refused(refusals.join("\n")));
    }
    if options.dry_run {
        writeln!(out, "Dry run: nothing was changed.").map_err(failed)?;
        return Ok(Outcome::DryRun(plan));
    }
    if !confirm() {
        return Err(Failure::NotConfirmed);
    }
    // Held until the end: no program starts on the installation meanwhile.
    let _locks = locks;

    let mut summary = Summary::default();
    if let Some(path) = &backup_path {
        writeln!(out, "Writing the backup…").map_err(failed)?;
        let written = backup::write(path, &plan.backup_sources)
            .map_err(|error| Failure::Failed(format!("the backup failed: {error}")))?;
        summary.backup = Some(written);
    }
    migrate(target, options.mover, &mut summary).map_err(Failure::Failed)?;
    print_summary(&summary, &plan, out).map_err(failed)?;
    Ok(Outcome::Migrated(summary))
}

/// The old entries whose owner is not the user running the tool. The
/// migration copies modes and rewrites files inside task storage that tasks
/// wrote: run by root over another user's files it would act with more
/// rights than the owner has, so it runs only as the owner.
fn owned_by_someone_else(plan: &Plan) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    plan.moves
        .iter()
        .map(|(source, _)| source)
        .filter(|source| fs::symlink_metadata(source).is_ok_and(|meta| meta.uid() != me))
        .cloned()
        .collect()
}

/// The moves themselves, after the checks and the backup.
fn migrate(target: &Target, mover: Mover, summary: &mut Summary) -> Result<(), String> {
    let mut controller_part = |controller: &ControllerMove| -> Result<(), String> {
        if let controller::Outcome::Migrated {
            moved,
            sessions,
            executions,
        } = controller::migrate(
            &controller.legacy,
            &controller.state,
            &controller.work,
            mover,
        )? {
            summary.moved.extend(moved);
            summary.sessions = sessions;
            summary.executions = executions;
        }
        Ok(())
    };
    match target {
        Target::Installation(installation) => {
            let moved = server::move_entries(&installation.old, &installation.server, mover)?;
            // A network server has no local controller: nothing to mark.
            if installation.controller.present() {
                controller_part(&installation.controller)?;
            }
            summary.moved.splice(0..0, moved);
            summary.deleted = server::delete_stale_files(&installation.old);
            mover::write_marker(installation.server.dir()).map_err(|error| {
                format!(
                    "cannot write the migration marker in {}: {error}",
                    installation.server.dir().display()
                )
            })
        }
        Target::Controller(controller) => controller_part(controller),
    }
}

fn print_plan(target: &Target, plan: &Plan, out: &mut dyn Write) -> io::Result<()> {
    let what = match target {
        Target::Installation(_) => "this installation",
        Target::Controller(_) => "this controller",
    };
    writeln!(out, "The files of {what} move to the per-binary layout:")?;
    for (from, to) in &plan.moves {
        writeln!(out, "  move    {} -> {}", from.display(), to.display())?;
    }
    for path in &plan.deletions {
        writeln!(out, "  delete  {}", path.display())?;
    }
    for path in &plan.left {
        writeln!(
            out,
            "  keep    {} (no longer read; delete it by hand)",
            path.display()
        )?;
    }
    writeln!(
        out,
        "Claude Code sessions and stored workspace paths of the moved tasks follow their storage."
    )?;
    if !plan.backup_sources.is_empty() {
        writeln!(out, "The backup holds:")?;
        for path in &plan.backup_sources {
            writeln!(out, "  {}", path.display())?;
        }
    }
    Ok(())
}

fn print_summary(summary: &Summary, plan: &Plan, out: &mut dyn Write) -> io::Result<()> {
    writeln!(out, "Migrated.")?;
    if let Some(backup) = &summary.backup {
        writeln!(
            out,
            "  backup: {} ({} entries, {})",
            backup.path.display(),
            backup.entries,
            human(backup.bytes)
        )?;
    }
    writeln!(out, "  moved: {}", list_or_none(&summary.moved))?;
    let deleted: Vec<String> = summary
        .deleted
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    writeln!(out, "  deleted: {}", list_or_none(&deleted))?;
    writeln!(
        out,
        "  Claude sessions renamed: {}; stored executions rewritten: {}",
        summary.sessions, summary.executions
    )?;
    for path in &plan.left {
        writeln!(out, "  left for you to delete: {}", path.display())?;
    }
    Ok(())
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "nothing".into()
    } else {
        items.join(", ")
    }
}

/// `bytes` in the largest binary unit that keeps a whole number.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
