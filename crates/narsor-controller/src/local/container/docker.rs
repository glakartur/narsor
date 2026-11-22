//! The Docker boundary of the local backend's container work mode (KTD2).
//!
//! The controller calls the `docker` CLI through [`DockerTransport`], not
//! through a Docker API crate — the way the repository calls `git`. The
//! production implementation, [`DockerCli`], runs `docker` with explicit
//! arguments and no shell; tests use the in-memory
//! [`FakeDocker`](super::docker_fake::FakeDocker) instead.
//!
//! The trait is synchronous: a call can last as long as an image build, so
//! callers run it off the async thread (`spawn_blocking` or a job thread).
//!
//! Only rootful Docker is supported: rootless Docker and Podman map the host
//! uid differently and break the ownership of task storage, so
//! [`DockerTransport::check_available`] refuses them.
//!
//! The container hardening of KTD5 is not a choice of the caller: a
//! [`RunRequest`] has no field for a privileged container or a host PID, IPC
//! or user namespace, and every container gets an init process, the host
//! network, no capabilities and `no-new-privileges`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

/// The program every call runs. There is no configurable CLI command in v1.
const DOCKER: &str = "docker";

/// How long a short call (`info`, `inspect`, `ls`, `rm`, `logs`, `run`)
/// may take before it counts as an unreachable daemon.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a `pull` or a `build` may take before it counts as an
/// unreachable daemon.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How often a running call is checked for its end.
const WAIT_STEP: Duration = Duration::from_millis(10);

/// How much of a failed command's output an error keeps (the end of it,
/// where a build log has its error).
const OUTPUT_TAIL_BYTES: usize = 4096;

/// The result of a transport call.
pub type DockerResult<T> = Result<T, DockerError>;

/// Why a transport call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerError {
    /// The `docker` program is not installed or not on `PATH`.
    NotInstalled,
    /// The daemon cannot be reached. A container is never reported missing
    /// because of this.
    Unreachable(String),
    /// The daemon runs rootless, which the work mode does not support.
    Rootless,
    /// The container or image does not exist.
    Missing(String),
    /// Any other failure: the command and what it said.
    Failed { command: String, message: String },
}

impl fmt::Display for DockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotInstalled => write!(
                f,
                "docker not found: install Docker and put the `docker` command on PATH"
            ),
            Self::Unreachable(message) => {
                write!(f, "the Docker daemon cannot be reached: {message}")
            }
            Self::Rootless => write!(
                f,
                "the Docker daemon runs rootless; the docker work mode needs rootful Docker \
                 (rootless Docker and Podman map the host user differently)"
            ),
            Self::Missing(message) => write!(f, "{message}"),
            Self::Failed { command, message } => write!(f, "`{command}` failed: {message}"),
        }
    }
}

impl std::error::Error for DockerError {}

/// The state of a container, as the backend reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    /// The container's process runs (Docker's `running`, also `paused`).
    Running,
    /// Created but not started yet (`created`, `restarting`).
    Starting,
    /// The process ended (`exited`, `dead`, `removing`) with this code.
    Exited(i32),
    /// No container has this name.
    Missing,
    /// The daemon cannot be reached, so the state is unknown.
    Unreachable(String),
}

/// What [`DockerTransport::inspect_image`] reports about an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInfo {
    /// The content id, `sha256:…`.
    pub id: String,
    pub labels: BTreeMap<String, String>,
}

/// One container event of [`DockerTransport::follow_container_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEvent {
    /// What happened (`start`, `die`, `destroy`, ...).
    pub action: String,
    /// The container's name.
    pub name: String,
    /// The container's labels, and what else Docker says of it.
    pub attributes: BTreeMap<String, String>,
}

/// The container events [`DockerCli`] follows: the ones that change what an
/// observation sees.
const FOLLOWED_EVENTS: [&str; 4] = ["start", "die", "destroy", "oom"];

/// One `docker build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// The build context directory.
    pub context: PathBuf,
    /// The Dockerfile; `None` uses `<context>/Dockerfile`.
    pub dockerfile: Option<PathBuf>,
    /// The tag the built image gets.
    pub tag: String,
    /// `--build-arg` values, in order.
    pub build_args: Vec<(String, String)>,
    pub labels: BTreeMap<String, String>,
}

/// A bind mount of a host path into the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

/// The host user the container runs as (KTD5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserIds {
    pub uid: u32,
    pub gid: u32,
}

/// One container to create and start, in one `docker run -d` call.
///
/// The hardening of KTD5 is fixed by [`run_args`]; this request has no way
/// to ask for a privileged container, a host PID, IPC or user namespace or
/// extra capabilities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub user: UserIds,
    pub mounts: Vec<Mount>,
    /// The complete environment of the container, as `KEY`/`VALUE` pairs.
    ///
    /// The values travel in the `docker` argv, which other local users can
    /// read in `ps`. That is acceptable because the environment is an
    /// allowlist of non-secret values (KTD7): secrets reach the container
    /// through a mounted secrets directory, never through this list.
    pub env: Vec<(String, String)>,
    pub workdir: Option<PathBuf>,
    /// The program the container runs; it replaces the image's entrypoint.
    pub program: String,
    pub args: Vec<String>,
}

/// The Docker operations the local backend needs.
pub trait DockerTransport: fmt::Debug + Send + Sync {
    /// Checks that a rootful daemon answers; returns its version.
    fn check_available(&self) -> DockerResult<String>;

    /// Pulls `reference` from its registry.
    fn pull_image(&self, reference: &str) -> DockerResult<()>;

    /// Builds and tags an image; returns what the tag now points at.
    fn build_image(&self, request: &BuildRequest) -> DockerResult<ImageInfo>;

    /// The image `reference` (a tag or an id) points at; `None` when there
    /// is none.
    fn inspect_image(&self, reference: &str) -> DockerResult<Option<ImageInfo>>;

    /// The ids of the images that carry every one of `labels` (each `key`
    /// or `key=value`).
    fn list_images(&self, labels: &[String]) -> DockerResult<Vec<String>>;

    /// Removes an image; an image that is already gone is not an error.
    fn remove_image(&self, reference: &str) -> DockerResult<()>;

    /// Creates and starts a container; returns its id.
    fn run_container(&self, request: &RunRequest) -> DockerResult<String>;

    /// The state of the container `name`. A missing container and an
    /// unreachable daemon are states, not errors.
    fn inspect_container(&self, name: &str) -> DockerResult<ContainerState>;

    /// Stops and removes a container; a container that is already gone is
    /// not an error.
    fn remove_container(&self, name: &str) -> DockerResult<()>;

    /// The last `tail` lines the container's process wrote, its standard
    /// output and error together.
    fn container_logs(&self, name: &str, tail: usize) -> DockerResult<String>;

    /// Follows the daemon's events of the containers carrying `label`
    /// (`key=value`) and hands each to `on_event`, until the stream ends
    /// (`Ok`) or `on_event` returns false. Blocks for as long as the stream
    /// lasts, which is meant to be forever: run it on a thread of its own.
    ///
    /// `on_event` may also get `None` while no event comes, just to tell
    /// whether it still wants them.
    fn follow_container_events(
        &self,
        label: &str,
        on_event: &mut dyn FnMut(Option<ContainerEvent>) -> bool,
    ) -> DockerResult<()>;
}

/// The transport that runs the `docker` CLI.
///
/// Every call has a deadline ([`CALL_TIMEOUT`], [`BUILD_TIMEOUT`] for a
/// pull or a build): a call that does not end in time is killed and reads as
/// an unreachable daemon, which callers treat as "try again later".
#[derive(Debug, Clone)]
pub struct DockerCli {
    program: PathBuf,
    call_timeout: Duration,
    build_timeout: Duration,
}

impl Default for DockerCli {
    fn default() -> Self {
        Self {
            program: PathBuf::from(DOCKER),
            call_timeout: CALL_TIMEOUT,
            build_timeout: BUILD_TIMEOUT,
        }
    }
}

impl DockerCli {
    /// A transport that runs `program` instead of `docker`, with these
    /// deadlines.
    #[cfg(test)]
    pub(crate) fn with_program(
        program: impl Into<PathBuf>,
        call_timeout: Duration,
        build_timeout: Duration,
    ) -> Self {
        Self {
            program: program.into(),
            call_timeout,
            build_timeout,
        }
    }

    /// Runs a short call and returns its standard output.
    fn docker<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> DockerResult<String> {
        docker_output(run(&self.program, args, self.call_timeout)?, args)
    }

    /// Runs a pull or a build and returns its standard output.
    fn docker_long<S: AsRef<std::ffi::OsStr>>(&self, args: &[S]) -> DockerResult<String> {
        docker_output(run(&self.program, args, self.build_timeout)?, args)
    }
}

impl DockerTransport for DockerCli {
    fn check_available(&self) -> DockerResult<String> {
        let output = self.docker(&[
            "info",
            "--format",
            r#"{"version":{{json .ServerVersion}},"security":{{json .SecurityOptions}}}"#,
        ])?;
        parse_info(&output)
    }

    fn pull_image(&self, reference: &str) -> DockerResult<()> {
        check_reference(reference)?;
        self.docker_long(&["pull", "--quiet", reference]).map(drop)
    }

    fn build_image(&self, request: &BuildRequest) -> DockerResult<ImageInfo> {
        check_reference(&request.tag)?;
        self.docker_long(&build_args(request))?;
        self.inspect_image(&request.tag)?.ok_or_else(|| {
            DockerError::Missing(format!(
                "the image {} is gone right after its build",
                request.tag
            ))
        })
    }

    fn inspect_image(&self, reference: &str) -> DockerResult<Option<ImageInfo>> {
        check_reference(reference)?;
        match self.docker(&["image", "inspect", reference]) {
            Ok(output) => parse_image_inspect(&output).map(Some),
            Err(DockerError::Missing(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_images(&self, labels: &[String]) -> DockerResult<Vec<String>> {
        let output = self.docker(&list_images_args(labels))?;
        let mut ids: Vec<String> = lines(&output);
        // One line per tag: an image with two tags is listed twice.
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    fn remove_image(&self, reference: &str) -> DockerResult<()> {
        check_reference(reference)?;
        match self.docker(&["image", "rm", reference]) {
            Ok(_) | Err(DockerError::Missing(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn run_container(&self, request: &RunRequest) -> DockerResult<String> {
        check_reference(&request.image)?;
        let output = self.docker(&run_args(request))?;
        Ok(output.trim().to_string())
    }

    fn inspect_container(&self, name: &str) -> DockerResult<ContainerState> {
        let output = self.docker(&inspect_container_args(name));
        container_state(output)
    }

    fn remove_container(&self, name: &str) -> DockerResult<()> {
        match self.docker(&remove_container_args(name)) {
            Ok(_) | Err(DockerError::Missing(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn follow_container_events(
        &self,
        label: &str,
        on_event: &mut dyn FnMut(Option<ContainerEvent>) -> bool,
    ) -> DockerResult<()> {
        let args = container_events_args(label);
        let mut child = Command::new(&self.program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| spawn_failure(&error, &args))?;
        let stderr = read_all(child.stderr.take().expect("stderr is piped"));
        let lines = read_lines(child.stdout.take().expect("stdout is piped"));
        let early = match pump_events(&lines, EVENTS_IDLE_TICK, on_event) {
            PumpEnd::Ended => None,
            PumpEnd::Unwanted => Some(Ok(())),
            PumpEnd::Failed(message) => Some(Err(DockerError::Failed {
                command: command_line(&args),
                message: format!("cannot read the docker events: {message}"),
            })),
        };
        if let Some(outcome) = early {
            // Still running: nobody reads it any more.
            let _ = child.kill();
            let _ = child.wait();
            return outcome;
        }
        let status = child.wait().map_err(|error| DockerError::Failed {
            command: command_line(&args),
            message: format!("cannot wait for docker: {error}"),
        })?;
        let stderr = stderr.join().unwrap_or_default();
        docker_output(
            CallOutput {
                status,
                stdout: Vec::new(),
                stderr,
            },
            &args,
        )
        .map(drop)
    }

    fn container_logs(&self, name: &str, tail: usize) -> DockerResult<String> {
        let args = container_logs_args(name, tail);
        // `docker logs` replays the container's standard error on its own
        // standard error, which is where a loader writes.
        let output = run(&self.program, &args, self.call_timeout)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            return Ok(tail_text(&format!("{stdout}{stderr}")));
        }
        Err(classify_failure(&command_line(&args), &tail_text(&stderr)))
    }
}

/// How long the event follower waits for a line before it asks its
/// callback (with `None`) whether events are still wanted: a quiet stream
/// still stops soon after its follower is no longer heard.
const EVENTS_IDLE_TICK: Duration = Duration::from_secs(1);

/// How [`pump_events`] ended.
#[derive(Debug, PartialEq, Eq)]
enum PumpEnd {
    /// The callback wanted no more events.
    Unwanted,
    /// The stream ended.
    Ended,
    /// Reading the stream failed.
    Failed(String),
}

/// The lines of `pipe`, read on a thread of their own until it ends, fails
/// or nobody receives them any more.
fn read_lines(pipe: impl io::Read + Send + 'static) -> mpsc::Receiver<io::Result<String>> {
    use std::io::BufRead as _;

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::BufReader::new(pipe).lines() {
            let failed = line.is_err();
            if sender.send(line).is_err() || failed {
                return;
            }
        }
    });
    receiver
}

/// Hands every container event of `lines` to `on_event`, and `None` after
/// each `idle` without one, until the callback returns false, the stream
/// ends or a read fails.
fn pump_events(
    lines: &mpsc::Receiver<io::Result<String>>,
    idle: Duration,
    on_event: &mut dyn FnMut(Option<ContainerEvent>) -> bool,
) -> PumpEnd {
    loop {
        let wanted = match lines.recv_timeout(idle) {
            Ok(Ok(line)) => match parse_container_event(&line) {
                Some(event) => on_event(Some(event)),
                None => {
                    tracing::debug!(%line, "docker events: an event that cannot be read");
                    continue;
                }
            },
            Ok(Err(error)) => return PumpEnd::Failed(error.to_string()),
            Err(mpsc::RecvTimeoutError::Timeout) => on_event(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => return PumpEnd::Ended,
        };
        if !wanted {
            return PumpEnd::Unwanted;
        }
    }
}

pub(crate) fn container_events_args(label: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "events".into(),
        "--format".into(),
        "{{json .}}".into(),
        "--filter".into(),
        "type=container".into(),
        "--filter".into(),
        format!("label={label}").into(),
    ];
    for event in FOLLOWED_EVENTS {
        args.push("--filter".into());
        args.push(format!("event={event}").into());
    }
    args
}

/// One line of `docker events --format '{{json .}}'`; `None` when it is no
/// container event.
pub(crate) fn parse_container_event(line: &str) -> Option<ContainerEvent> {
    let event: Value = serde_json::from_str(line).ok()?;
    if event["Type"] != "container" {
        return None;
    }
    let actor = &event["Actor"];
    let attributes: BTreeMap<String, String> = actor["Attributes"]
        .as_object()
        .map(|attributes| {
            attributes
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(ContainerEvent {
        action: event["Action"].as_str()?.to_string(),
        name: attributes.get("name").cloned().unwrap_or_default(),
        attributes,
    })
}

fn list_images_args(labels: &[String]) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["image", "ls", "--quiet", "--no-trunc"]
        .into_iter()
        .map(OsString::from)
        .collect();
    for label in labels {
        // Docker requires every `label` filter to match.
        args.push("--filter".into());
        args.push(format!("label={label}").into());
    }
    args
}

pub(crate) fn container_logs_args(name: &str, tail: usize) -> Vec<OsString> {
    vec![
        "logs".into(),
        "--tail".into(),
        tail.to_string().into(),
        name.into(),
    ]
}

/// The `docker` arguments that create and start the container of `request`
/// with the hardening of KTD5.
pub(crate) fn run_args(request: &RunRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["run", "--detach", "--pull", "never", "--name"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.push(request.name.clone().into());
    for (key, value) in &request.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
    args.push("--user".into());
    args.push(format!("{}:{}", request.user.uid, request.user.gid).into());
    for fixed in [
        "--init",
        "--network",
        "host",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
    ] {
        args.push(fixed.into());
    }
    for mount in &request.mounts {
        args.push("--mount".into());
        args.push(mount_spec(mount));
    }
    for (key, value) in &request.env {
        args.push("--env".into());
        args.push(format!("{key}={value}").into());
    }
    if let Some(workdir) = &request.workdir {
        args.push("--workdir".into());
        args.push(workdir.into());
    }
    args.push("--entrypoint".into());
    args.push(request.program.clone().into());
    args.push(request.image.clone().into());
    args.extend(request.args.iter().map(OsString::from));
    args
}

pub(crate) fn inspect_container_args(name: &str) -> Vec<OsString> {
    ["container", "inspect", name]
        .into_iter()
        .map(OsString::from)
        .collect()
}

pub(crate) fn remove_container_args(name: &str) -> Vec<OsString> {
    ["container", "rm", "--force", "--volumes", name]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn build_args(request: &BuildRequest) -> Vec<OsString> {
    // `--force-rm`: the legacy builder keeps the container of a failed
    // step otherwise; BuildKit accepts the flag and has no such container.
    let mut args: Vec<OsString> = vec![
        "build".into(),
        "--force-rm".into(),
        "--tag".into(),
        request.tag.clone().into(),
    ];
    if let Some(dockerfile) = &request.dockerfile {
        args.push("--file".into());
        args.push(dockerfile.into());
    }
    for (key, value) in &request.build_args {
        args.push("--build-arg".into());
        args.push(format!("{key}={value}").into());
    }
    for (key, value) in &request.labels {
        args.push("--label".into());
        args.push(format!("{key}={value}").into());
    }
    args.push(request.context.clone().into());
    args
}

/// A `--mount` value. Docker reads it as one CSV record, so a field with a
/// comma or a quote is quoted.
fn mount_spec(mount: &Mount) -> OsString {
    let mut fields = vec![
        "type=bind".to_string(),
        csv_field(&format!("source={}", mount.source.display())),
        csv_field(&format!("target={}", mount.target.display())),
    ];
    if mount.read_only {
        fields.push("readonly".to_string());
    }
    fields.join(",").into()
}

fn csv_field(field: &str) -> String {
    if field.contains([',', '"']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// A reference that starts with `-` would be read as an option.
fn check_reference(reference: &str) -> DockerResult<()> {
    if reference.is_empty() || reference.starts_with('-') {
        return Err(DockerError::Failed {
            command: DOCKER.to_string(),
            message: format!("{reference:?} is not an image reference"),
        });
    }
    Ok(())
}

/// What a finished call printed and how it ended.
struct CallOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `program` with `args`, in a process group of its own, for at most
/// `timeout`. A call that does not end in time is killed with its group and
/// reads as an unreachable daemon.
fn run<S: AsRef<std::ffi::OsStr>>(
    program: &std::path::Path,
    args: &[S],
    timeout: Duration,
) -> DockerResult<CallOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| spawn_failure(&error, args))?;
    // Both pipes are read while the call runs, so a call with much output
    // never blocks on a full pipe.
    let stdout = read_all(child.stdout.take().expect("stdout is piped"));
    let stderr = read_all(child.stderr.take().expect("stderr is piped"));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(WAIT_STEP),
            Ok(None) => {
                kill_group(&mut child);
                // The readers end when the killed group closes the pipes;
                // nobody waits for them.
                return Err(DockerError::Unreachable(format!(
                    "`{}` did not answer within {timeout:?}",
                    command_line(args)
                )));
            }
            Err(error) => {
                kill_group(&mut child);
                return Err(DockerError::Failed {
                    command: command_line(args),
                    message: format!("cannot wait for docker: {error}"),
                });
            }
        }
    };
    Ok(CallOutput {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn read_all(mut pipe: impl io::Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn kill_group(child: &mut std::process::Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: a plain signal to the process group the child leads.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The standard output of a call that succeeded, else its error.
fn docker_output<S: AsRef<std::ffi::OsStr>>(
    output: CallOutput,
    args: &[S],
) -> DockerResult<String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = if stderr.trim().is_empty() {
        tail_text(&String::from_utf8_lossy(&output.stdout))
    } else {
        tail_text(&stderr)
    };
    Err(classify_failure(&command_line(args), &message))
}

fn spawn_failure<S: AsRef<std::ffi::OsStr>>(error: &io::Error, args: &[S]) -> DockerError {
    if error.kind() == io::ErrorKind::NotFound {
        DockerError::NotInstalled
    } else {
        DockerError::Failed {
            command: command_line(args),
            message: format!("cannot start docker: {error}"),
        }
    }
}

fn command_line<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> String {
    // The words before the first option name the operation; the values
    // after them need not repeat in every log line.
    let words: Vec<_> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .take_while(|word| !word.starts_with('-'))
        .collect();
    format!("{DOCKER} {}", words.join(" "))
}

/// Maps what a failed `docker` command said to an error.
pub(crate) fn classify_failure(command: &str, stderr: &str) -> DockerError {
    let lower = stderr.to_lowercase();
    let message = stderr.trim().to_string();
    const UNREACHABLE: [&str; 4] = [
        "cannot connect to the docker daemon",
        "is the docker daemon running",
        "failed to connect to the docker api",
        "permission denied while trying to connect to the docker",
    ];
    const MISSING: [&str; 3] = ["no such container", "no such object", "no such image"];
    if UNREACHABLE.iter().any(|marker| lower.contains(marker)) {
        DockerError::Unreachable(message)
    } else if MISSING.iter().any(|marker| lower.contains(marker)) {
        DockerError::Missing(message)
    } else {
        DockerError::Failed {
            command: command.to_string(),
            message,
        }
    }
}

/// Reads the `docker info` template output of [`DockerCli::check_available`].
pub(crate) fn parse_info(output: &str) -> DockerResult<String> {
    let info: Value =
        serde_json::from_str(output.trim()).map_err(|error| unexpected("info", error))?;
    let rootless = info["security"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|option| option.contains("rootless"));
    if rootless {
        return Err(DockerError::Rootless);
    }
    info["version"]
        .as_str()
        .filter(|version| !version.is_empty())
        .map(str::to_string)
        .ok_or_else(|| unexpected("info", "no server version"))
}

fn parse_image_inspect(output: &str) -> DockerResult<ImageInfo> {
    let image = first_object(output, "image inspect")?;
    let id = image["Id"]
        .as_str()
        .ok_or_else(|| unexpected("image inspect", "no Id"))?
        .to_string();
    let labels = image["Config"]["Labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
        .collect();
    Ok(ImageInfo { id, labels })
}

/// The container state from the outcome of `docker container inspect`.
pub(crate) fn container_state(output: DockerResult<String>) -> DockerResult<ContainerState> {
    let output = match output {
        Ok(output) => output,
        Err(DockerError::Missing(_)) => return Ok(ContainerState::Missing),
        Err(DockerError::Unreachable(message)) => return Ok(ContainerState::Unreachable(message)),
        Err(error) => return Err(error),
    };
    let container = first_object(&output, "container inspect")?;
    let state = &container["State"];
    let code = || {
        state["ExitCode"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .unwrap_or(-1)
    };
    match state["Status"].as_str() {
        Some("running" | "paused") => Ok(ContainerState::Running),
        Some("created" | "restarting") => Ok(ContainerState::Starting),
        Some("exited" | "dead" | "removing") => Ok(ContainerState::Exited(code())),
        other => Err(unexpected(
            "container inspect",
            format!("unknown status {other:?}"),
        )),
    }
}

/// The first element of the JSON array `docker … inspect` prints.
fn first_object(output: &str, what: &str) -> DockerResult<Value> {
    let parsed: Value =
        serde_json::from_str(output.trim()).map_err(|error| unexpected(what, error))?;
    match parsed {
        Value::Array(mut items) if !items.is_empty() => Ok(items.swap_remove(0)),
        _ => Err(unexpected(what, "an empty answer")),
    }
}

fn unexpected(what: &str, detail: impl fmt::Display) -> DockerError {
    DockerError::Failed {
        command: format!("{DOCKER} {what}"),
        message: format!("unexpected output: {detail}"),
    }
}

fn lines(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn tail_text(text: &str) -> String {
    let text = text.trim();
    if text.len() <= OUTPUT_TAIL_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - OUTPUT_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn run_builds_a_hardened_container_from_the_request() {
        let request = RunRequest {
            name: "narsor-task-1".into(),
            image: "narsor/task:abc".into(),
            labels: BTreeMap::from([("narsor.task".into(), "t1".into())]),
            user: UserIds {
                uid: 1000,
                gid: 100,
            },
            mounts: vec![
                Mount {
                    source: "/srv/tasks/t1".into(),
                    target: "/srv/tasks/t1".into(),
                    read_only: false,
                },
                Mount {
                    source: "/home/u/odd,dir".into(),
                    target: "/opt/odd,dir".into(),
                    read_only: true,
                },
            ],
            env: vec![("TERM".into(), "xterm".into())],
            workdir: Some("/srv/tasks/t1".into()),
            program: "/opt/narsor/narsor-runner".into(),
            args: vec!["--task".into(), "t1".into()],
        };

        let args = strings(run_args(&request));

        assert_eq!(
            args,
            [
                "run",
                "--detach",
                "--pull",
                "never",
                "--name",
                "narsor-task-1",
                "--label",
                "narsor.task=t1",
                "--user",
                "1000:100",
                "--init",
                "--network",
                "host",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--mount",
                "type=bind,source=/srv/tasks/t1,target=/srv/tasks/t1",
                "--mount",
                "type=bind,\"source=/home/u/odd,dir\",\"target=/opt/odd,dir\",readonly",
                "--env",
                "TERM=xterm",
                "--workdir",
                "/srv/tasks/t1",
                "--entrypoint",
                "/opt/narsor/narsor-runner",
                "narsor/task:abc",
                "--task",
                "t1",
            ]
        );
        for forbidden in ["--privileged", "--pid", "--ipc", "--userns", "--cap-add"] {
            assert!(!args.iter().any(|arg| arg.starts_with(forbidden)));
        }
    }

    #[test]
    fn a_docker_events_line_reads_as_the_containers_event() {
        let args = strings(container_events_args("io.narsor.managed=true"));
        assert!(args.starts_with(&["events".into(), "--format".into(), "{{json .}}".into()]));
        assert!(args.contains(&"label=io.narsor.managed=true".to_string()));
        assert!(args.contains(&"event=die".to_string()));

        let line = r#"{"status":"die","id":"abc","from":"narsor-task:1","Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"exitCode":"137","io.narsor.managed":"true","name":"narsor-local-1"}},"scope":"local","time":1}"#;
        let event = parse_container_event(line).expect("a container event");
        assert_eq!(event.action, "die");
        assert_eq!(event.name, "narsor-local-1");
        assert_eq!(event.attributes["io.narsor.managed"], "true");

        let network = r#"{"Type":"network","Action":"connect","Actor":{"ID":"n","Attributes":{}}}"#;
        assert_eq!(parse_container_event(network), None);
        assert_eq!(parse_container_event("not json"), None);
    }

    /// A quiet stream that stays open still asks whether events are wanted,
    /// so its follower stops; a stream that cannot be read stops too.
    #[test]
    fn the_event_pump_stops_on_a_quiet_open_stream_and_on_a_read_error() {
        let event =
            r#"{"Type":"container","Action":"die","Actor":{"ID":"a","Attributes":{"name":"n"}}}"#;
        let (sender, lines) = mpsc::channel();
        sender.send(Ok(event.to_string())).unwrap();
        let mut seen = Vec::new();
        let end = pump_events(&lines, Duration::from_millis(10), &mut |event| {
            seen.push(event.map(|event| event.action));
            seen.len() < 2
        });
        assert_eq!(end, PumpEnd::Unwanted);
        assert_eq!(seen, [Some("die".to_string()), None]);
        drop(sender);

        let (sender, lines) = mpsc::channel();
        sender.send(Err(io::Error::other("broken pipe"))).unwrap();
        let end = pump_events(&lines, Duration::from_millis(10), &mut |_| true);
        assert_eq!(end, PumpEnd::Failed("broken pipe".into()));
    }

    #[test]
    fn inspect_and_remove_address_the_container_by_name() {
        assert_eq!(
            strings(inspect_container_args("narsor-task-1")),
            ["container", "inspect", "narsor-task-1"]
        );
        assert_eq!(
            strings(remove_container_args("narsor-task-1")),
            ["container", "rm", "--force", "--volumes", "narsor-task-1"]
        );
        assert_eq!(
            strings(container_logs_args("narsor-task-1", 50)),
            ["logs", "--tail", "50", "narsor-task-1"]
        );
    }

    fn inspected(status: &str, exit_code: i64) -> DockerResult<String> {
        Ok(format!(
            r#"[{{"Id":"abc","State":{{"Status":"{status}","Running":false,"ExitCode":{exit_code}}}}}]"#
        ))
    }

    #[test]
    fn inspect_output_maps_to_the_container_state() {
        let cases = [
            ("running", inspected("running", 0), ContainerState::Running),
            ("created", inspected("created", 0), ContainerState::Starting),
            (
                "exited",
                inspected("exited", 137),
                ContainerState::Exited(137),
            ),
            ("dead", inspected("dead", 1), ContainerState::Exited(1)),
            (
                "no such container",
                Err(classify_failure(
                    "docker container",
                    "Error response from daemon: No such container: narsor-task-1",
                )),
                ContainerState::Missing,
            ),
            (
                "unreachable daemon",
                Err(classify_failure(
                    "docker container",
                    "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                     Is the docker daemon running?",
                )),
                ContainerState::Unreachable(
                    "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                     Is the docker daemon running?"
                        .into(),
                ),
            ),
        ];
        for (case, output, expected) in cases {
            assert_eq!(container_state(output), Ok(expected), "{case}");
        }
    }

    #[test]
    fn stderr_of_a_failed_command_is_classified() {
        let cases = [
            (
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                 Is the docker daemon running?",
                "unreachable",
            ),
            (
                "failed to connect to the docker API at unix:///nonexist; check if the path \
                 is correct and if the daemon is running",
                "unreachable",
            ),
            (
                "Error response from daemon: No such image: narsor/task:abc",
                "missing",
            ),
            ("Error: No such object: narsor-task-1", "missing"),
            (
                "Error response from daemon: Conflict. The container name is already in use",
                "failed",
            ),
        ];
        for (stderr, expected) in cases {
            let kind = match classify_failure("docker run", stderr) {
                DockerError::Unreachable(_) => "unreachable",
                DockerError::Missing(_) => "missing",
                DockerError::Failed { .. } => "failed",
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(kind, expected, "{stderr}");
        }
    }

    #[test]
    fn availability_check_refuses_rootless_docker() {
        assert_eq!(
            parse_info(r#"{"version":"29.1.0","security":["name=seccomp,profile=builtin"]}"#),
            Ok("29.1.0".to_string())
        );
        assert_eq!(
            parse_info(
                r#"{"version":"29.1.0","security":["name=seccomp,profile=builtin","name=rootless"]}"#
            ),
            Err(DockerError::Rootless)
        );
    }

    #[test]
    fn a_call_that_does_not_answer_in_time_is_killed_and_reads_as_unreachable() {
        let dir = std::env::temp_dir().join(format!("narsor-docker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("docker");
        // A `docker` that hangs, with a child that holds its pipes open.
        std::fs::write(&program, "#!/bin/sh\nsleep 30 &\nsleep 30\n").unwrap();
        std::fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let cli = DockerCli::with_program(
            &program,
            Duration::from_millis(200),
            Duration::from_millis(400),
        );

        let started = Instant::now();
        let short = cli.check_available();
        let long = cli.pull_image("debian:trixie-slim");
        let elapsed = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(&short, Err(DockerError::Unreachable(message))
                if message.contains("`docker info` did not answer within 200ms")),
            "{short:?}"
        );
        assert!(
            matches!(&long, Err(DockerError::Unreachable(message))
                if message.contains("`docker pull` did not answer within 400ms")),
            "{long:?}"
        );
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[test]
    fn a_missing_docker_binary_reads_as_not_installed() {
        let error = spawn_failure(&io::Error::from(io::ErrorKind::NotFound), &["info"]);

        assert_eq!(error, DockerError::NotInstalled);
        assert!(error.to_string().contains("docker not found"));
    }
}
