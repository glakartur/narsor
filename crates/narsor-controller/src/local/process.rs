//! Runners as host processes: every runner of the process work mode, and
//! the read-only runner (D63) in both modes.
//!
//! A runner runs in its own process group and survives the controller,
//! like a Pod survives its controller (R14); removing compute kills the
//! group. Only the instance that started a runner can reap it and learn its
//! exit code ([`ProcessRunners`] keeps those children); a runner started by
//! a previous controller is found through its `workload.json`, and its pid
//! together with its start time tells whether it still runs.
//!
//! A runner inherits the controller's environment less the controller's
//! own `NARSOR_*` settings (R12) and, for a control plane that is not
//! trusted, less the user's credential variables (R17).
//!
//! # Killing a group whose runner already died
//!
//! The group outlives its leader while anything the runner started (a
//! harness, a tool) still runs, and such an orphan could keep writing the
//! task storage under the next runner. Its id is the dead runner's pid,
//! though, which the system may hand out again once the group is gone, and
//! a stranger could lead a new group of that id. So the group is signalled
//! only when one of its live members carries the workload's mark: the
//! environment variable [`WORKLOAD_MARK_ENV`] set to the workload's uid,
//! which every runner gets and its descendants inherit. A group is either
//! all ours or all a stranger's, and a pid cannot be reused while a group
//! of that id still has members, so a marked member proves the group ours.
//! A runner started before the mark existed, or descendants that cleared
//! their environment, are left alone once the runner is gone, as before.
//!
//! # The reader
//!
//! A read-only runner (`narsor-runner --read-only`, D63) is a second kind of
//! workload of the same task: its own directory under `readers/`, its own
//! process group and its own mark, so removing a task's working runner never
//! signals the reader and removing the reader never signals a working
//! runner's group. It takes the storage's lock like any runner, so it cannot
//! run beside a working one; the controller sees to that first and the lock
//! is the second guard. It never looks at the process group a dead runner
//! left behind: should one still hold the lock, the reader cannot read and
//! says so.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use narsor_controller_runtime::backend::{
    BackendError, BackendResult, TaskResources, WorkloadLaunch, WorkloadPhase, WorkloadState,
};
use narsor_shared::config::{CLAUDE_CONFIG_DIR_ENV, CODEX_HOME_ENV, ENV_PREFIX};
use narsor_shared::git::USER_GIT_ACCESS_ENVS;

use super::backend::LocalBackendConfig;
use super::workload::{
    io_error, task_args, ExitRecord, LastRunner, Look, Role, TaskRecord, WorkloadKind,
    WorkloadRecord, WorkloadStore, WORKLOAD_MARK_ENV,
};

/// How long removing compute waits for a killed runner to disappear before
/// it answers "in progress" and lets the controller come back.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// The runners of the adapter that are host processes. Cloning shares the
/// process table.
#[derive(Debug, Clone)]
pub(super) struct ProcessRunners {
    config: Arc<LocalBackendConfig>,
    store: WorkloadStore,
    /// Runners this instance started: only a parent can learn an exit code
    /// and reap its child. A runner started by a previous controller is
    /// found through its `workload.json` instead.
    children: Arc<Mutex<HashMap<String, Child>>>,
}

impl ProcessRunners {
    /// Runners started as `config` says, recorded in `store`.
    pub(super) fn new(config: Arc<LocalBackendConfig>, store: WorkloadStore) -> Self {
        Self {
            config,
            store,
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The pid of the task's runner of `role` while it runs as a host
    /// process.
    pub(super) fn live_pid(&self, task: &TaskResources, role: Role) -> Option<u32> {
        let record = self.store.read_record(task, role).ok()??;
        let WorkloadKind::Process { pid, start_ticks } = record.kind else {
            return None;
        };
        match self.look(task, role, &record, pid, start_ticks) {
            Ok(Look {
                phase: Some(phase), ..
            }) if phase.is_alive() => Some(pid),
            _ => None,
        }
    }

    fn children(&self) -> MutexGuard<'_, HashMap<String, Child>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The phase of the process `pid` of `record`, with the record to write
    /// back when the look learned of an exit.
    pub(super) fn look(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
        pid: u32,
        start_ticks: Option<u64>,
    ) -> BackendResult<Look> {
        let (phase, exit) = self.phase(task, role, pid, start_ticks, record.exited)?;
        Ok(Look {
            phase: Some(phase),
            updated: exit.map(|exit| WorkloadRecord {
                exited: Some(exit),
                ..record.clone()
            }),
        })
    }

    /// A process's phase, and whether the record learned something new (an
    /// exit) that must be written back.
    fn phase(
        &self,
        task: &TaskResources,
        role: Role,
        pid: u32,
        start_ticks: Option<u64>,
        exited: Option<ExitRecord>,
    ) -> BackendResult<(WorkloadPhase, Option<ExitRecord>)> {
        // The parent asks its child first: only it can reap the process and
        // learn the exit code, whatever another instance recorded.
        let name = role.name(task);
        let mut children = self.children();
        if let Some(child) = children.get_mut(name) {
            if child.id() == pid {
                return match child.try_wait() {
                    Ok(Some(status)) => {
                        children.remove(name);
                        let exit = ExitRecord {
                            code: status.code(),
                        };
                        Ok((WorkloadPhase::Exited { code: exit.code }, Some(exit)))
                    }
                    Ok(None) => Ok((WorkloadPhase::Running, None)),
                    Err(error) => Err(io_error("wait for the runner", error)),
                };
            }
        }
        drop(children);
        if let Some(exit) = exited {
            return Ok((WorkloadPhase::Exited { code: exit.code }, None));
        }
        // Started by an earlier controller: alive is all that can be told;
        // its exit code went to whoever reaped it.
        if process_alive(pid, start_ticks) {
            Ok((WorkloadPhase::Running, None))
        } else {
            let exit = ExitRecord { code: None };
            Ok((WorkloadPhase::Exited { code: None }, Some(exit)))
        }
    }

    /// Starts the task's working runner on `storage`, warning first when
    /// the task last ran in a container (R3).
    pub(super) fn start_working(
        &self,
        task: &TaskResources,
        launch: &WorkloadLaunch,
        storage: &Path,
    ) -> BackendResult<WorkloadState> {
        let previous = self.note_start(task);
        let workload = self.spawn(task, Role::Work, launch, storage)?;
        if let Some(previous) = previous {
            self.remember_start(task, previous);
        }
        Ok(workload)
    }

    /// R3: warns when a task whose last runner was a container now starts
    /// as a host process; returns its per-task record then.
    fn note_start(&self, task: &TaskResources) -> Option<TaskRecord> {
        let previous = self
            .store
            .read_task_record(task)
            .filter(|record| record.kind == LastRunner::Container)?;
        tracing::warn!(
            task = %task.task_id,
            "local backend: task {} last ran in a Docker container and now starts as a host \
             process (controller.local.mode is 'process'): it runs as your user with your HOME \
             settings, and tools installed only in its image are missing",
            task.task_id
        );
        Some(previous)
    }

    /// Records that the task's runner is a host process now.
    fn remember_start(&self, task: &TaskResources, previous: TaskRecord) {
        let record = TaskRecord {
            kind: LastRunner::Process,
            ..previous
        };
        if let Err(error) = self.store.write_task_record(task, &record) {
            tracing::warn!(task = %task.task_id, %error, "local backend: cannot update the task record");
        }
    }

    /// The command line of a runner process: a reader gets `--read-only`.
    pub(super) fn runner_args(
        &self,
        role: Role,
        launch: &WorkloadLaunch,
        dir: &Path,
        storage: &Path,
    ) -> Vec<OsString> {
        let mut args = task_args(
            launch.task_id.as_str(),
            storage,
            dir,
            &launch.control_plane_url,
            &launch.incarnation,
        );
        if role == Role::Read {
            args.push("--read-only".into());
        }
        args.extend(self.config.runner_args.iter().map(Into::into));
        args
    }

    /// Starts the task's runner of `role` as a process in its own group,
    /// in a new workload directory.
    pub(super) fn spawn(
        &self,
        task: &TaskResources,
        role: Role,
        launch: &WorkloadLaunch,
        storage: &Path,
    ) -> BackendResult<WorkloadState> {
        use std::os::unix::process::CommandExt;

        let dir = self.store.prepare_workload_dir(task, role, launch)?;
        let log = fs::File::create(dir.join("runner.log"))
            .map_err(|error| io_error("create the runner log", error))?;
        let log_err = log
            .try_clone()
            .map_err(|error| io_error("create the runner log", error))?;
        let uid = format!("local-{}", uuid::Uuid::new_v4().simple());
        let mut command = Command::new(&self.config.program);
        command.args(self.runner_args(role, launch, &dir, storage));
        // Nothing of the controller's own settings — its enrollment code
        // least of all — reaches the runner or the tools it starts (R12);
        // what the adapter sets itself is set below. `spawn` isolates it:
        // the controller's environment stays as it is.
        // Nor, for a server that is not trusted, the user's own logins: a
        // harness directory or a Git credential inherited from the shell.
        let withhold = self.config.withhold_user_credentials;
        for (name, _) in std::env::vars_os() {
            if is_controller_setting(&name) || (withhold && is_user_credential(&name)) {
                command.env_remove(name);
            }
        }
        command
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .env(WORKLOAD_MARK_ENV, &uid)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            // Its own group: removing the workload takes everything the
            // runner started with it, and the controller's own signals
            // (a Ctrl-C) do not reach it.
            .process_group(0);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(BackendError::rejected(format!(
                    "cannot start '{}': {error}",
                    self.config.program.display()
                )));
            }
        };
        let record = WorkloadRecord {
            uid,
            resource_generation: 1,
            incarnation: launch.incarnation.clone(),
            launch_generation: launch.generation,
            kind: WorkloadKind::Process {
                pid: child.id(),
                start_ticks: process_start_ticks(child.id()),
            },
            exited: None,
        };
        self.children().insert(role.name(task).to_string(), child);
        self.store.write_record(task, role, &record)?;
        Ok(WorkloadState {
            uid: record.uid,
            resource_generation: record.resource_generation,
            incarnation: record.incarnation,
            launch_generation: record.launch_generation,
            phase: WorkloadPhase::Running,
        })
    }

    /// Kills the process `pid` of `record` and what is left of its group;
    /// true once both are gone, false while they are still going.
    pub(super) fn stop(
        &self,
        task: &TaskResources,
        role: Role,
        record: &WorkloadRecord,
        pid: u32,
        start_ticks: Option<u64>,
    ) -> BackendResult<bool> {
        let (phase, _) = self.phase(task, role, pid, start_ticks, record.exited)?;
        if phase.is_alive() {
            // SAFETY: signalling the process group this adapter created;
            // `phase` checked the pid still names that process.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
            if !self.wait_gone(task, role, pid, start_ticks) {
                return Ok(false);
            }
        }
        // The runner is gone; what it started may not be (see the module
        // docs on why the mark makes the group safe to signal).
        if !kill_marked_group(pid, &record.uid) {
            return Ok(false);
        }
        self.children().remove(role.name(task));
        Ok(true)
    }

    /// Waits briefly for a killed runner to go, reaping it when it is ours.
    fn wait_gone(
        &self,
        task: &TaskResources,
        role: Role,
        pid: u32,
        start_ticks: Option<u64>,
    ) -> bool {
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            let gone = match self.children().get_mut(role.name(task)) {
                Some(child) => child.try_wait().map(|s| s.is_some()).unwrap_or(true),
                None => !process_alive(pid, start_ticks),
            };
            if gone || Instant::now() >= deadline {
                return gone;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Whether `name` is one of the controller's own `NARSOR_*` variables,
/// which a runner never inherits.
fn is_controller_setting(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(ENV_PREFIX.as_bytes())
        && name != OsStr::new(narsor_shared::config::AGENT_ARGS_ENV)
}

/// Variables through which a runner or a harness would reach the user's
/// own logins, beyond those git reads ([`USER_GIT_ACCESS_ENVS`]).
const USER_CREDENTIAL_ENVS: [&str; 13] = [
    CODEX_HOME_ENV,
    CLAUDE_CONFIG_DIR_ENV,
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_AUTH_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_CONFIG_DIR",
    "GITLAB_TOKEN",
    "GL_TOKEN",
    "GLAB_TOKEN",
    "GLAB_CONFIG_DIR",
];

/// The prefix of git's configuration variables (`GIT_CONFIG_GLOBAL`,
/// `GIT_CONFIG_KEY_<n>`, ...), any of which can hand git a credential.
const GIT_CONFIG_ENV_PREFIX: &str = "GIT_CONFIG";

/// Whether `name` is one of the user's credential variables, which a runner
/// serving a server that is not trusted never inherits.
fn is_user_credential(name: &OsStr) -> bool {
    let name = name.as_encoded_bytes();
    name.starts_with(GIT_CONFIG_ENV_PREFIX.as_bytes())
        || USER_GIT_ACCESS_ENVS
            .iter()
            .chain(&USER_CREDENTIAL_ENVS)
            .any(|known| name == known.as_bytes())
}

/// Field 22 of `/proc/{pid}/stat`: when the process started.
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; fields resume after
    // the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    if state == "Z" {
        return None;
    }
    // `rest` starts at field 3 (state); field 22 is 19 further on.
    fields.nth(18)?.parse().ok()
}

/// Kills what is left of the process group `pgid` when it is the
/// workload's (a live member carries `uid` as [`WORKLOAD_MARK_ENV`]), and
/// waits briefly for it to go; true when no marked member is left.
fn kill_marked_group(pgid: u32, uid: &str) -> bool {
    let deadline = Instant::now() + KILL_WAIT;
    loop {
        if !group_marked(pgid, uid) {
            return true;
        }
        // SAFETY: a marked live member proves the group ours, and its id
        // cannot be reused while that member lives.
        unsafe {
            libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
        }
        if Instant::now() >= deadline {
            return !group_marked(pgid, uid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a live (not zombie) member of process group `pgid` carries the
/// workload's mark. A process whose environment cannot be read (another
/// user's) does not.
fn group_marked(pgid: u32, uid: &str) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    let mark = format!("{WORKLOAD_MARK_ENV}={uid}");
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            return false;
        };
        let environ = fs::read(format!("/proc/{pid}/environ")).ok();
        member_marked(process_group(pid), pgid, environ.as_deref(), &mark)
    })
}

/// Whether a process — in group `member_group` (`None` for a zombie or one
/// that is gone), with environment `environ` (`None` when unreadable) —
/// is a live member of group `pgid` carrying `mark`.
fn member_marked(member_group: Option<u32>, pgid: u32, environ: Option<&[u8]>, mark: &str) -> bool {
    member_group == Some(pgid) && environ.is_some_and(|environ| environ_has(environ, mark))
}

/// Whether the NUL-separated `environ` block holds exactly the entry `mark`.
fn environ_has(environ: &[u8], mark: &str) -> bool {
    environ
        .split(|byte| *byte == 0)
        .any(|entry| entry == mark.as_bytes())
}

/// Field 5 of `/proc/{pid}/stat`: its process group; `None` for a zombie or
/// a process that is gone.
fn process_group(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    if fields.next()? == "Z" {
        return None;
    }
    // `rest` starts at field 3 (state); field 5 is two further on.
    fields.nth(1)?.parse().ok()
}

/// Whether `pid` still names the process that started at `start_ticks`
/// and has not terminated (a zombie counts as terminated).
pub(super) fn process_alive(pid: u32, start_ticks: Option<u64>) -> bool {
    match (process_start_ticks(pid), start_ticks) {
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_controllers_settings_are_not_inherited_by_a_runner() {
        for name in [
            "NARSOR_ENROLLMENT_CODE",
            "NARSOR_CONTROL_PLANE",
            "NARSOR_RUNNER_CONTROL_PLANE",
        ] {
            assert!(is_controller_setting(name.as_ref()), "{name}");
        }
        assert!(!is_controller_setting(
            narsor_shared::config::AGENT_ARGS_ENV.as_ref()
        ));
        for name in ["PATH", "HOME", "RUST_LOG", "narsor_lower", "XNARSOR_"] {
            assert!(!is_controller_setting(name.as_ref()), "{name}");
        }
    }

    #[test]
    fn the_users_logins_are_credential_variables() {
        for name in [
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_CONFIG_DIR",
            "GITLAB_TOKEN",
            "GL_TOKEN",
            "GLAB_TOKEN",
            "GLAB_CONFIG_DIR",
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_KEY_0",
            "XDG_CONFIG_HOME",
        ] {
            assert!(is_user_credential(name.as_ref()), "{name}");
        }
        for name in ["PATH", "HOME", "GIT_TERMINAL_PROMPT", "codex_home"] {
            assert!(!is_user_credential(name.as_ref()), "{name}");
        }
    }

    #[test]
    fn only_a_live_member_with_the_mark_belongs_to_the_group() {
        let mark = format!("{WORKLOAD_MARK_ENV}=local-ours");
        let ours = mark.as_bytes();
        let stranger = format!("{WORKLOAD_MARK_ENV}=local-someone-else");

        assert!(member_marked(Some(7), 7, Some(ours), &mark));
        assert!(!member_marked(Some(8), 7, Some(ours), &mark), "other group");
        assert!(!member_marked(None, 7, Some(ours), &mark), "zombie or gone");
        assert!(!member_marked(Some(7), 7, None, &mark), "unreadable");
        assert!(!member_marked(Some(7), 7, Some(stranger.as_bytes()), &mark));
        assert!(!member_marked(Some(7), 7, Some(b"A=1"), &mark), "unmarked");
    }

    #[test]
    fn only_an_exact_entry_is_the_workloads_mark() {
        let mark = format!("{WORKLOAD_MARK_ENV}=local-ours");
        let environ = |entries: &[&str]| entries.join("\0").into_bytes();

        assert!(environ_has(&environ(&["A=1", &mark, "B=2"]), &mark));
        assert!(!environ_has(&environ(&["A=1"]), &mark));
        assert!(!environ_has(&environ(&[]), &mark));
        let other = format!("{WORKLOAD_MARK_ENV}=local-someone-else");
        assert!(!environ_has(&environ(&[&other]), &mark));
        let longer = format!("{mark}-2");
        assert!(!environ_has(&environ(&[&longer]), &mark));
    }
}
