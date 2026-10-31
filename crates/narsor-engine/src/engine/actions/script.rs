//! `ScriptAction` executor (`docs/dev/workflow-schema.md` §3.3.B).
//!
//! Runs a command in the workspace and turns the way it ended into an output
//! label. The label is decided in the order §3.3.B prescribes:
//!
//! 1. stdout parses as a JSON object carrying a `choice` key — the answer an
//!    agent gives (KTD3, [`super::answer`]): that label wins, and brings its
//!    `reason`, `variables`, `artifacts`, `additional_data` and
//!    `additional_artifacts` along;
//! 2. otherwise the exit code is looked up in `exit_codes`;
//! 3. otherwise the documented default applies: `0` → `success`, anything
//!    else → `failure`.
//!
//! A process that never returned an exit code — killed by a signal — is an
//! action error, not a quiet `failure`: nothing about it says the workflow
//! should take the failure transition rather than stop and be looked at.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::answer::{read_choice_answer, ChoiceAnswer};
use super::{ActionExecutor, ActionOutcome, ActionRequest, TimedOutAction, ACTION_ID_ENV_VAR};
use crate::domain::{ActionConfig, ActionError, ActionResult, ScriptActionConfig};

/// How much of stdout and of stderr is kept in the [`ActionResult`], in bytes.
///
/// The result is persisted into `state.json` on every step, so unbounded logs
/// would grow the execution record without bound and slow every later load.
/// 64 KiB keeps the tail of a normal build log — enough to read what happened —
/// while full logs belong in artifacts.
pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;

/// Appended in place of everything that [`MAX_CAPTURED_OUTPUT_BYTES`] cut off.
pub const TRUNCATION_MARKER: &str = "\n… [truncated by narsor]";

/// Hard ceiling on how much of a pipe is held in memory at once, in bytes.
///
/// [`MAX_CAPTURED_OUTPUT_BYTES`] bounds what is *persisted*, but it is applied
/// after the fact, so reading a pipe to its end first would let a script that
/// prints without bound exhaust narsor's memory before any cap could
/// be applied — the captured-output limit would give a false sense of safety.
/// Past this ceiling the pipe is still drained, so the child never blocks on a
/// full pipe. Old bytes are dropped to retain the latest diagnostics.
pub const MAX_BUFFERED_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Label for exit code 0 when `exit_codes` does not say otherwise.
pub const DEFAULT_SUCCESS_LABEL: &str = "success";

/// Label for any non-zero exit code when `exit_codes` does not say otherwise.
pub const DEFAULT_FAILURE_LABEL: &str = "failure";

/// Executes `ScriptAction` states.
///
/// Stateless — one instance serves the whole engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptExecutor;

impl ScriptExecutor {
    /// Creates an executor.
    pub fn new() -> Self {
        Self
    }
}

impl ActionExecutor for ScriptExecutor {
    fn execute<'a>(
        &'a self,
        request: ActionRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ActionOutcome, ActionError>> + Send + 'a>> {
        Box::pin(async move {
            let config = script_config(request.action)?;
            let working_dir = resolve_working_dir(&request.workspace.path, config);
            let limit = request
                .timeout
                .or_else(|| config.timeout_secs.map(Duration::from_secs));

            run(config, &working_dir, request.action_execution_id, limit).await
        })
    }
}

/// Narrows the action configuration to a script configuration.
fn script_config(action: &ActionConfig) -> Result<&ScriptActionConfig, ActionError> {
    match action {
        ActionConfig::Script(config) => Ok(config),
        other => Err(ActionError {
            message: format!(
                "ScriptExecutor received a {} action",
                action_type_name(other)
            ),
            error_type: Some("wrong_action_type".to_string()),
            details: None,
        }),
    }
}

/// The schema name of an action type, for error messages.
fn action_type_name(action: &ActionConfig) -> &'static str {
    match action {
        ActionConfig::Agent(_) => "agent",
        ActionConfig::Script(_) => "script",
        ActionConfig::Human(_) => "human",
        ActionConfig::Wait(_) => "wait",
    }
}

/// The directory the command runs in: the workspace, or `working_dir` resolved
/// against it. An absolute `working_dir` is taken as given.
fn resolve_working_dir(workspace_path: &str, config: &ScriptActionConfig) -> PathBuf {
    let workspace = PathBuf::from(workspace_path);

    match &config.working_dir {
        Some(dir) => {
            let dir = Path::new(dir);
            if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                workspace.join(dir)
            }
        }
        None => workspace,
    }
}

/// Spawns the command, waits for it (or for the time limit) and builds the
/// outcome.
async fn run(
    config: &ScriptActionConfig,
    working_dir: &Path,
    action_execution_id: &str,
    limit: Option<Duration>,
) -> Result<ActionOutcome, ActionError> {
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(working_dir)
        .envs(&config.env)
        .env(ACTION_ID_ENV_VAR, action_execution_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(config, working_dir, &error))?;

    // Taken out of the child so the pipes can be drained while it runs: a
    // process that fills its stdout pipe blocks until somebody reads it, which
    // would otherwise look like a hang and trip the time limit.
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

    let wait = async {
        tokio::join!(
            read_capped(&mut stdout_pipe),
            read_capped(&mut stderr_pipe),
            child.wait(),
        )
    };

    let finished = match limit {
        Some(limit) => tokio::time::timeout(limit, wait).await,
        None => Ok(wait.await),
    };

    let (stdout_read, stderr_read, status) = match finished {
        Ok(finished) => finished,
        Err(_elapsed) => {
            // The waiting future has been dropped, so the child is free again:
            // kill it and reap it, leaving no orphan behind.
            let _ = child.kill().await;

            return Ok(ActionOutcome::TimedOut(TimedOutAction {
                timeout: limit.unwrap_or_default(),
                subject: format!("script '{}'", config.command),
            }));
        }
    };

    let stdout_buffer = stdout_read.map_err(|error| io_error(config, "stdout", &error))?;
    let stderr_buffer = stderr_read.map_err(|error| io_error(config, "stderr", &error))?;
    let status = status.map_err(|error| io_error(config, "exit status", &error))?;

    let stdout = String::from_utf8_lossy(&stdout_buffer).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buffer).into_owned();

    let Some(exit_code) = status.code() else {
        return Err(signal_error(config, &status, &stderr));
    };

    let mut result = match parse_json_output(&stdout) {
        Some(answer) => answer
            .map_err(|reason| invalid_output_error(config, reason))?
            .into_result(),
        None => ActionResult::new(label_from_exit_code(exit_code, &config.exit_codes)),
    };
    result.exit_code = Some(exit_code);
    result.stdout = truncate(&stdout);
    result.stderr = truncate(&stderr);

    Ok(ActionOutcome::Completed(result))
}

/// Drains `pipe` to its end, retaining at most the latest [`MAX_BUFFERED_OUTPUT_BYTES`].
///
/// Reading continues past the ceiling so a chatty child is never blocked by a
/// full pipe; only the retained bytes are bounded.
pub(crate) async fn read_capped<R>(pipe: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = VecDeque::new();
    let mut chunk = [0_u8; 16 * 1024];

    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            return Ok(buffer.into_iter().collect());
        }
        for byte in &chunk[..read] {
            if buffer.len() == MAX_BUFFERED_OUTPUT_BYTES {
                buffer.pop_front();
            }
            buffer.push_back(*byte);
        }
    }
}

/// The label an exit code maps to: `exit_codes` first, then the documented
/// default of `0` → `success` and anything else → `failure`.
fn label_from_exit_code(exit_code: i32, exit_codes: &HashMap<i32, String>) -> String {
    if let Some(label) = exit_codes.get(&exit_code) {
        return label.clone();
    }

    if exit_code == 0 {
        DEFAULT_SUCCESS_LABEL.to_string()
    } else {
        DEFAULT_FAILURE_LABEL.to_string()
    }
}

/// Reads the answer object (KTD3) from stdout.
///
/// Returns `None` — never an error — when stdout is not JSON at all (an
/// ordinary log line) or is JSON without a string `choice`. Scripts are
/// explicitly allowed to print whatever they like and let the exit code
/// decide. An answer with a `choice` but a field of the wrong shape is an
/// error: it was meant as an answer, and part of it would be lost.
fn parse_json_output(stdout: &str) -> Option<Result<ChoiceAnswer, &'static str>> {
    let payload: Value = serde_json::from_str(stdout.trim()).ok()?;
    read_choice_answer(payload.as_object()?)
}

/// Keeps the tail of captured output within [`MAX_CAPTURED_OUTPUT_BYTES`].
///
/// Build and test failures commonly appear at the end of a long log. The cut
/// lands on a character boundary, so the result stays valid UTF-8.
pub(crate) fn truncate(output: &str) -> String {
    if output.len() <= MAX_CAPTURED_OUTPUT_BYTES {
        return output.to_string();
    }

    let mut start = output.len() - MAX_CAPTURED_OUTPUT_BYTES;
    while !output.is_char_boundary(start) {
        start += 1;
    }

    format!("{}{TRUNCATION_MARKER}", &output[start..])
}

/// The command could not be started at all.
fn spawn_error(
    config: &ScriptActionConfig,
    working_dir: &Path,
    error: &std::io::Error,
) -> ActionError {
    ActionError {
        message: format!(
            "failed to start '{}' in {}: {error}",
            config.command,
            working_dir.display()
        ),
        error_type: Some("spawn_failed".to_string()),
        details: Some(serde_json::json!({
            "command": config.command,
            "args": config.args,
            "working_dir": working_dir.display().to_string(),
            "os_error": error.raw_os_error(),
        })),
    }
}

/// The script answered with a `choice`, but the rest of its answer is not
/// usable.
fn invalid_output_error(config: &ScriptActionConfig, reason: &str) -> ActionError {
    ActionError {
        message: format!(
            "the answer '{}' printed could not be used: {reason}",
            config.command
        ),
        error_type: Some("invalid_script_output".to_string()),
        details: Some(serde_json::json!({ "command": config.command })),
    }
}

/// Reading a pipe or reaping the child failed.
fn io_error(config: &ScriptActionConfig, what: &str, error: &std::io::Error) -> ActionError {
    ActionError {
        message: format!("failed to read {what} of '{}': {error}", config.command),
        error_type: Some("io_failed".to_string()),
        details: Some(serde_json::json!({ "command": config.command })),
    }
}

/// The process died from a signal and left no exit code to map.
fn signal_error(
    config: &ScriptActionConfig,
    status: &std::process::ExitStatus,
    stderr: &str,
) -> ActionError {
    let signal = signal_of(status);

    let message = match signal {
        Some(signal) => format!(
            "'{}' was killed by signal {signal} and produced no exit code",
            config.command
        ),
        None => format!("'{}' ended without an exit code", config.command),
    };

    ActionError {
        message,
        error_type: Some("killed_by_signal".to_string()),
        details: Some(serde_json::json!({
            "command": config.command,
            "signal": signal,
            "stderr": truncate(stderr),
        })),
    }
}

/// The signal that killed the process, where the platform reports one.
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }

    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}
