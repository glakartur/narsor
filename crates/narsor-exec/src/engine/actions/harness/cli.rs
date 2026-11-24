//! The process mechanics every CLI harness shares (R8): the operator's extra
//! arguments, how the prompt travels, and one harness process from spawn to
//! a parsed result.
//!
//! A CLI harness builds its own command line (its flags, without the
//! prompt) and hands it here with the final prompt. From then on all CLI
//! harnesses behave the same:
//!
//! * the operator's extra arguments replace the same flags and go after the
//!   harness's own, right where the prompt goes ([`apply_extra_args`]);
//! * a prompt up to [`LONGEST_PROMPT_ARGUMENT`] is the last argument, a
//!   longer one goes on stdin (R5) — Linux refuses a single argument over
//!   `MAX_ARG_STRLEN` (128 KiB) with E2BIG (NARSOR-1124-001);
//! * the process leads a group of its own, both pipes are drained line by
//!   line while it runs, every stdout line goes to the parser and then to
//!   the observers, and after the final answer the group gets the grace
//!   period, then `SIGTERM`, then `SIGKILL`.

use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use narsor_protocol::chat::{
    summary_line, ChatElement, TechnicalCategory, UnclassifiedReason, SUMMARY_MAX_CHARS,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::request::{AgentRequest, AgentRunEnding, FinishedRun, RunObservers, RunStarted};
use super::{FinalAnswer, HarnessStreamParser};
use crate::domain::ActionError;
use crate::engine::actions::process_group::{self, GroupGuard};
use crate::engine::actions::script::{truncate, MAX_BUFFERED_OUTPUT_BYTES};
use crate::engine::actions::session::conversation::record_stdout_line;
use crate::engine::actions::session::PARSE_LINE_LIMIT;
use crate::engine::actions::ACTION_ID_ENV_VAR;
use crate::persistence::ConversationSource;

/// The longest prompt passed as a command-line argument; a longer one goes
/// on stdin. The margin leaves room below Linux's 128 KiB per argument.
pub(crate) const LONGEST_PROMPT_ARGUMENT: usize = 96 * 1024;

/// A harness's command line without the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandLine {
    /// The binary to run; looked up on `PATH` unless it is a path.
    pub(crate) program: String,
    /// Its flags, each one separately — never a shell string. The prompt
    /// (or [`Self::stdin_marker`]) goes after them.
    pub(crate) args: Vec<String>,
    /// The argument that tells the harness to read the prompt from stdin,
    /// for a harness that needs one (`codex exec -`). `None` when a missing
    /// prompt argument is enough.
    pub(crate) stdin_marker: Option<&'static str>,
}

impl CommandLine {
    /// The command line `program args…`.
    pub(crate) fn new(program: &str, args: Vec<String>) -> Self {
        Self {
            program: program.to_string(),
            args,
            stdin_marker: None,
        }
    }
}

/// Runs `command` with `prompt` for `request`: the context files, extra
/// arguments, prompt transport, the process and the parse loop. `started`,
/// with any model or effort the extra arguments override, goes to the
/// observers once the process is running.
pub(crate) async fn run(
    harness: &str,
    mut command: CommandLine,
    started: RunStarted,
    prompt: String,
    request: &AgentRequest,
    parser: Box<dyn HarnessStreamParser>,
    observers: &RunObservers<'_>,
) -> Result<AgentRunEnding, ActionError> {
    let prompt = request.prompt_with_context(prompt).await?;
    apply_extra_args(&mut command.args, &request.extra_args);
    let started = with_extra_args_overrides(started, &request.extra_args);
    let stdin = place_prompt(harness, &mut command, prompt);
    spawn_and_wait(&command, stdin, started, request, parser, observers).await
}

/// Puts `prompt` on the command line when it fits; otherwise adds the stdin
/// marker, if the harness has one, and returns the prompt for stdin.
fn place_prompt(harness: &str, command: &mut CommandLine, prompt: String) -> Option<String> {
    if prompt.len() <= LONGEST_PROMPT_ARGUMENT {
        command.args.push(prompt);
        return None;
    }
    tracing::warn!(
        harness,
        prompt_bytes = prompt.len(),
        limit_bytes = LONGEST_PROMPT_ARGUMENT,
        "the prompt is too long for one command-line argument; it goes on stdin"
    );
    if let Some(marker) = command.stdin_marker {
        command.args.push(marker.to_string());
    }
    Some(prompt)
}

/// Puts the operator's extra arguments after the harness's own flags. An
/// extra flag replaces the same flag (and the values it takes) that the
/// harness chose, so the operator's word wins on a conflict.
///
/// A flag is a token starting with `-`; the tokens after it, up to the next
/// flag, are its values. Avoid flags that take a variable number of values:
/// they would swallow the prompt that follows.
pub(crate) fn apply_extra_args(args: &mut Vec<String>, extra: &[String]) {
    if extra.is_empty() {
        return;
    }
    let mut groups: Vec<(&str, usize)> = Vec::new();
    for token in extra {
        if token.starts_with('-') {
            let (name, values) = match token.split_once('=') {
                Some((name, _)) => (name, 1),
                None => (token.as_str(), 0),
            };
            groups.push((name, values));
        } else if let Some(last) = groups.last_mut() {
            if !last.0.is_empty() && !extra_group_has_inline_value(extra, last.0) {
                last.1 += 1;
            }
        }
    }

    let mut kept: Vec<String> = Vec::with_capacity(args.len() + extra.len());
    let mut skip = 0usize;
    for arg in args.drain(..) {
        if skip > 0 && !arg.starts_with('-') {
            skip -= 1;
            continue;
        }
        skip = 0;
        if arg.starts_with('-') {
            let (name, inline) = match arg.split_once('=') {
                Some((name, _)) => (name, true),
                None => (arg.as_str(), false),
            };
            if let Some((_, values)) = groups.iter().find(|(flag, _)| *flag == name) {
                skip = if inline { 0 } else { *values };
                continue;
            }
        }
        kept.push(arg);
    }
    kept.extend(extra.iter().cloned());
    *args = kept;
}

/// `started` with the model and the effort the operator's extra arguments
/// set, since they replace the harness's own flags: `--model`/`-m`,
/// `--effort`, and codex's `-c model_reasoning_effort=…`.
fn with_extra_args_overrides(mut started: RunStarted, extra: &[String]) -> RunStarted {
    for (index, token) in extra.iter().enumerate() {
        if !token.starts_with('-') {
            continue;
        }
        let (flag, value) = match token.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (
                token.as_str(),
                extra
                    .get(index + 1)
                    .map(String::as_str)
                    .filter(|value| !value.starts_with('-')),
            ),
        };
        let Some(value) = value else {
            continue;
        };
        match flag {
            "--model" | "-m" => started.model = Some(value.to_string()),
            "--effort" => started.effort = Some(value.to_string()),
            "-c" | "--config" => {
                if let Some(level) = value.strip_prefix("model_reasoning_effort=") {
                    started.effort = Some(level.trim_matches('"').to_string());
                }
            }
            _ => {}
        }
    }
    started
}

/// Whether `flag` was written as `flag=value` in `extra`.
fn extra_group_has_inline_value(extra: &[String], flag: &str) -> bool {
    extra
        .iter()
        .any(|token| token.split_once('=').is_some_and(|(name, _)| name == flag))
}

/// Summarizes one stderr line while preserving the original bytes separately.
pub(crate) fn stderr_element(line: &[u8]) -> ChatElement {
    let prefix = &line[..line.len().min(STDERR_SUMMARY_SOURCE_BYTES)];
    ChatElement::Technical {
        category: TechnicalCategory::Stderr,
        summary: summary_line(&String::from_utf8_lossy(prefix)),
        data: None,
    }
}

const STDERR_SUMMARY_SOURCE_BYTES: usize = (SUMMARY_MAX_CHARS + 1) * 4;

/// How long the group has to leave on `SIGTERM` before it is killed.
const TERM_TO_KILL: Duration = Duration::from_secs(3);

/// How long the pipes are read for after the group was killed.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(2);

/// Whether a question, like a choice, ends the step shortly after it.
///
/// Off until a run against the real `claude` shows that `--resume` works on
/// a session whose process was killed right after the question; the answer
/// to the question is that resume.
const KILL_AFTER_QUESTION: bool = false;

/// What [`spawn_and_wait`]'s driving loop found out about the group.
struct Driven {
    /// What the pipes and the wait ended with; `None` when the pipes did not
    /// close even after the group was killed.
    joined: Option<Joined>,
    /// The harness was still running when the grace period ran out.
    forced: bool,
    /// The group still had members when it was signalled.
    terminated: bool,
}

type Joined = (io::Result<()>, io::Result<()>, io::Result<ExitStatus>);

/// Runs the harness, draining both pipes while it goes and feeding every
/// stdout line to `parser` and then, with the elements it returned, to the
/// observers — or, for a usage report, its limits to the usage relay
/// ([`record_stdout_line`]).
///
/// Reading is a line loop ([`stream_lines`]) rather than `read_capped` (used
/// by the script executor, which has no per-line structure to preserve): a
/// chatty agent — and every agent is chatty — would otherwise block on a full
/// pipe and look exactly like a hang, so both pipes are still drained
/// concurrently with the wait, only now line by line instead of as one
/// unbounded blob.
///
/// The harness leads a process group of its own. Once the parser has seen the
/// final answer the step does not wait for the process to exit: after the
/// grace period the group is sent `SIGTERM` and then `SIGKILL`. The group is
/// cleaned up even when the harness exits by itself — a background task it
/// left behind keeps the stdout pipe open, so the pipe would never reach end
/// of file.
async fn spawn_and_wait(
    command_line: &CommandLine,
    stdin_content: Option<String>,
    started: RunStarted,
    request: &AgentRequest,
    parser: Box<dyn HarnessStreamParser>,
    observers: &RunObservers<'_>,
) -> Result<AgentRunEnding, ActionError> {
    let program = command_line.program.as_str();
    let working_dir = request.working_dir.as_path();
    let conversation = observers.conversation;
    let usage = observers.usage;

    let mut command = Command::new(program);
    command.args(&command_line.args).current_dir(working_dir);
    if let Some(action_execution_id) = &request.action_execution_id {
        command.env(ACTION_ID_ENV_VAR, action_execution_id);
    }
    command
        .stdin(if stdin_content.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let mut child = spawn_retrying_text_busy(&mut command)
        .await
        .map_err(|error| spawn_error(program, working_dir, &error))?;
    let mut group = GroupGuard::new(child.id());
    observers.notify_started(started);

    // Written alongside the pipe readers, not before them: a harness that
    // prints much before it has read its whole prompt would otherwise block
    // on a full stdout pipe while narsor blocks on a full stdin pipe.
    if let (Some(content), Some(mut stdin)) = (stdin_content, child.stdin.take()) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(content.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

    let mut parser = parser;
    let mut stderr_capture: Vec<u8> = Vec::new();
    // Kept here, not in the stdout task: the sender must outlive the task, or
    // the driving loop would see a closed channel where it expects an answer.
    let (answered_tx, mut answered_rx) = tokio::sync::watch::channel(false);

    let stdout_task = stream_lines(&mut stdout_pipe, |line| {
        let (elements, subagent) = if line.len() <= PARSE_LINE_LIMIT {
            let elements = parser.on_stdout_line(line);
            (elements, parser.last_line_subagent())
        } else {
            let too_large = vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::TooLarge,
            }];
            (too_large, None)
        };
        record_stdout_line(
            parser.as_mut(),
            conversation,
            usage,
            line,
            &elements,
            subagent.as_ref(),
        );
        let ends_step = match parser.final_answer() {
            Some(FinalAnswer::Choice) => true,
            Some(FinalAnswer::AwaitingInput) => KILL_AFTER_QUESTION,
            None => false,
        };
        if ends_step {
            answered_tx.send_replace(true);
        }
    });

    let stderr_task = stream_lines(&mut stderr_pipe, |line| {
        conversation.append(
            ConversationSource::Stderr,
            line,
            &[stderr_element(line)],
            None,
        );
        accumulate_capped(&mut stderr_capture, line, MAX_BUFFERED_OUTPUT_BYTES);
        accumulate_capped(&mut stderr_capture, b"\n", MAX_BUFFERED_OUTPUT_BYTES);
    });

    let grace = request.final_answer_grace;
    let pgid = group.pgid();
    let drive = async {
        let wait = async { tokio::join!(stdout_task, stderr_task, child.wait()) };
        tokio::pin!(wait);

        let mut joined: Option<Joined> = None;
        tokio::select! {
            finished = &mut wait => joined = Some(finished),
            _ = answered_rx.wait_for(|answered| *answered) => {}
        }
        let answered = *answered_rx.borrow();
        if joined.is_none() {
            joined = tokio::time::timeout(grace, &mut wait).await.ok();
        }
        let forced = joined.is_none();

        let mut terminated = false;
        if let Some(pgid) = pgid.filter(|_| answered) {
            if forced {
                terminated = process_group::signal(pgid, process_group::TERMINATE);
                if terminated {
                    joined = tokio::time::timeout(TERM_TO_KILL, &mut wait).await.ok();
                }
            }
            terminated |= process_group::signal(pgid, process_group::KILL);
            if joined.is_none() {
                joined = tokio::time::timeout(DRAIN_AFTER_KILL, &mut wait).await.ok();
            }
        }
        Driven {
            joined,
            forced,
            terminated,
        }
    };

    let driven = match request.timeout {
        Some(limit) => tokio::time::timeout(limit, drive).await,
        None => Ok(drive.await),
    };

    let driven = match driven {
        Ok(driven) => driven,
        Err(_elapsed) => {
            // The waiting future is gone, so the child can be reaped for
            // good. The still armed guard kills the rest of the group.
            let _ = child.kill().await;
            return Ok(AgentRunEnding::TimedOut);
        }
    };
    group.disarm();

    let Driven {
        joined,
        forced,
        terminated,
    } = driven;
    let (stdout_result, stderr_result, status) = match joined {
        Some(joined) => joined,
        // The pipes stayed open after the kill: a process outside the group
        // holds them. What was read so far is all there will be.
        None => (Ok(()), Ok(()), child.wait().await),
    };

    stdout_result.map_err(|error| io_error(program, "stdout", &error))?;
    stderr_result.map_err(|error| io_error(program, "stderr", &error))?;
    let status = status.map_err(|error| io_error(program, "exit status", &error))?;

    let stderr = String::from_utf8_lossy(&stderr_capture).into_owned();

    // A harness ended after its final answer was not a failure, whatever
    // status the signal left.
    let exit_code = match status.code() {
        _ if forced => 0,
        Some(code) => code,
        None => {
            return Err(ActionError {
                message: format!("the agent harness '{program}' was killed and left no exit code"),
                error_type: Some("killed_by_signal".to_string()),
                details: Some(
                    serde_json::json!({ "program": program, "stderr": truncate(&stderr) }),
                ),
            });
        }
    };

    let run = parser.finish(exit_code, &stderr);

    Ok(AgentRunEnding::Finished(FinishedRun {
        run,
        exit_code,
        stderr,
        terminated_group: terminated.then_some(pgid).flatten(),
    }))
}

/// The hard ceiling on one unterminated line's accumulated size, independent
/// of [`PARSE_LINE_LIMIT`] (which only bounds what the *parser* sees, not
/// what [`stream_lines`] holds while waiting for the next `\n`).
///
/// A real tool-result line runs to single-digit megabytes at most; a harness
/// that streams for an entire timeout window with no newline at all is not a
/// line at any size R14 was written for, and letting this accumulator grow
/// without bound turns a misbehaving subprocess into an OOM instead of the
/// ordinary "this action failed" outcome every other harness failure
/// produces. 256 MiB is generous headroom above any realistic single line
/// while still being a bound.
const MAX_LINE_BYTES: usize = 256 * 1024 * 1024;

/// Reads `pipe` line by line, calling `on_line` with the raw bytes of each
/// complete line — the trailing `\n` stripped and nothing else touched, not
/// even a trailing `\r`, not even invalid UTF-8. The final, unterminated
/// chunk left at EOF (the trace of a harness that never printed a last
/// newline) is delivered the same way as any other line.
///
/// Deliberately not `tokio::io::AsyncBufReadExt::lines()` (requires valid
/// UTF-8, and has no size limit of its own) and not
/// `tokio_util::codec::LinesCodec` (silently drops anything past its
/// configured length): this is the `fill_buf`/`consume` loop the two would
/// otherwise hide, kept explicit so that a line of any size still reaches
/// `on_line` in full (R14) — only *parsing* it is ever capped, by the caller,
/// never this function's own reading. The one exception is
/// [`MAX_LINE_BYTES`]: past that, this stops treating the stream as
/// well-formed and reports an error instead of accumulating forever.
async fn stream_lines<R>(pipe: &mut R, mut on_line: impl FnMut(&[u8])) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(64 * 1024, pipe);
    let mut line: Vec<u8> = Vec::new();

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }

        match available.iter().position(|&byte| byte == b'\n') {
            Some(pos) => {
                line.extend_from_slice(&available[..pos]);
                reader.consume(pos + 1);
                on_line(&line);
                line.clear();
            }
            None => {
                let consumed = available.len();
                line.extend_from_slice(available);
                reader.consume(consumed);
                if line.len() > MAX_LINE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("a single line exceeded {MAX_LINE_BYTES} bytes with no newline"),
                    ));
                }
            }
        }
    }

    if !line.is_empty() {
        on_line(&line);
    }
    Ok(())
}

/// Appends as much of `chunk` as fits within `cap` total bytes, silently
/// dropping the rest once the cap is reached — a smaller, line-oriented
/// cousin of `read_capped`'s "keep draining, stop retaining" rule, used
/// here only for the bounded stderr text quoted in error messages. The
/// conversation file holds the unbounded original regardless.
fn accumulate_capped(buffer: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    if buffer.len() >= cap {
        return;
    }
    let room = cap - buffer.len();
    buffer.extend_from_slice(&chunk[..chunk.len().min(room)]);
}

/// How many times a fresh `ETXTBSY` is retried before it is given up on as a
/// real error.
const SPAWN_TEXT_BUSY_RETRIES: u32 = 20;

/// Spawns `command`, retrying briefly on `ExecutableFileBusy`.
///
/// Linux (and other Unixes) refuse to `execve` a file that some other open
/// file description still has open for writing — ordinarily this means a
/// build is genuinely in flight, but it is also a real, if brief, race for a
/// binary that was just written and closed a moment ago and is executed
/// again immediately after (a harness deployed by a rolling update; the test
/// suite's own fixture, copied and renamed into place right before this same
/// process runs it — see `tests/actions_agent.rs`'s `FakeHarness::install`).
/// The window is a matter of microseconds, so a handful of immediate retries
/// clears it without ever masking a binary that is actually, persistently
/// busy — those keep failing past the retry budget exactly as before.
async fn spawn_retrying_text_busy(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    const RETRY_DELAY_MS: u64 = 5;

    for attempt in 0..SPAWN_TEXT_BUSY_RETRIES {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if attempt + 1 < SPAWN_TEXT_BUSY_RETRIES
                    && error.kind() == io::ErrorKind::ExecutableFileBusy =>
            {
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            }
            // On the last attempt, an `ExecutableFileBusy` gets a message
            // naming the retry budget already spent, so this is
            // distinguishable from a first-attempt failure with the same
            // raw OS error.
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "still busy after {SPAWN_TEXT_BUSY_RETRIES} attempts over ~{}ms: {error}",
                        u64::from(SPAWN_TEXT_BUSY_RETRIES - 1) * RETRY_DELAY_MS
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop above always returns before exhausting its retries")
}

/// The harness could not be started.
///
/// A missing binary gets its own message: `No such file or directory` on its
/// own tells an operator nothing about *which* file, and the answer is always
/// the same — install the harness, or put it on `PATH`.
fn spawn_error(program: &str, working_dir: &Path, error: &std::io::Error) -> ActionError {
    let message = if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "the agent harness binary '{program}' was not found; it is a runtime \
             prerequisite and must be installed and on PATH (or named by an \
             absolute path)"
        )
    } else {
        format!(
            "failed to start the agent harness '{program}' in {}: {error}",
            working_dir.display()
        )
    };

    ActionError {
        message,
        error_type: Some(if error.kind() == std::io::ErrorKind::NotFound {
            "harness_not_found".to_string()
        } else {
            "spawn_failed".to_string()
        }),
        details: Some(serde_json::json!({
            "program": program,
            "working_dir": working_dir.display().to_string(),
            "os_error": error.raw_os_error(),
        })),
    }
}

/// Reading a pipe or reaping the harness failed.
fn io_error(program: &str, what: &str, error: &std::io::Error) -> ActionError {
    ActionError {
        message: format!("failed to read {what} of the agent harness '{program}': {error}"),
        error_type: Some("io_failed".to_string()),
        details: Some(serde_json::json!({ "program": program })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn an_extra_flag_replaces_the_same_flag_and_its_value() {
        let mut args = strings(&["--print", "--effort", "low", "--model", "m"]);
        apply_extra_args(&mut args, &strings(&["--effort", "high"]));
        assert_eq!(args, ["--print", "--model", "m", "--effort", "high"]);
    }

    #[test]
    fn the_inline_form_replaces_too() {
        let mut args = strings(&["--effort", "low"]);
        apply_extra_args(&mut args, &strings(&["--effort=high"]));
        assert_eq!(args, ["--effort=high"]);
    }

    #[test]
    fn the_operator_model_and_effort_are_the_ones_reported_as_started() {
        let chosen = RunStarted {
            model: Some("chosen-model".to_string()),
            effort: Some("low".to_string()),
        };
        for (extra, model, effort) in [
            (vec!["--model", "operator-model"], "operator-model", "low"),
            (vec!["--effort=high"], "chosen-model", "high"),
            (
                vec![
                    "-m",
                    "operator-model",
                    "-c",
                    "model_reasoning_effort=\"xhigh\"",
                ],
                "operator-model",
                "xhigh",
            ),
        ] {
            let started = with_extra_args_overrides(chosen.clone(), &strings(&extra));
            assert_eq!(started.model.as_deref(), Some(model), "{extra:?}");
            assert_eq!(started.effort.as_deref(), Some(effort), "{extra:?}");
        }
    }

    #[test]
    fn a_short_prompt_is_the_last_argument_and_a_long_one_goes_on_stdin() {
        let long = "p".repeat(200 * 1024);
        for (prompt, marker, args, on_stdin) in [
            ("short", Some("-"), vec!["--json", "short"], false),
            (long.as_str(), Some("-"), vec!["--json", "-"], true),
            (long.as_str(), None, vec!["--json"], true),
        ] {
            let mut command = CommandLine::new("h", strings(&["--json"]));
            command.stdin_marker = marker;
            let stdin = place_prompt("h", &mut command, prompt.to_string());
            assert_eq!(command.args, args);
            assert_eq!(stdin.is_some(), on_stdin);
        }
    }
}
