use super::conversation::record_stdout_line;
use super::*;
use crate::engine::actions::harness::copilot_sdk::{
    run_copilot_sdk, CopilotSdkBackend, GitHubCopilotSdkRuntime,
};
use narsor_protocol::chat::{summary_line, TechnicalCategory, SUMMARY_MAX_CHARS};

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

/// A harness process that ran to completion, already parsed.
pub(crate) struct FinishedRun {
    /// What the adapter's stream parser distilled from the stdout stream.
    pub(crate) run: HarnessRun,
    /// The process's exit code.
    pub(crate) exit_code: i32,
    /// A bounded capture of stderr, for error messages only — the full text,
    /// unbounded, is what the conversation file holds (R14).
    pub(crate) stderr: String,
    /// The process group that was still alive after the final answer and was
    /// ended by narsor, when there was one.
    pub(crate) terminated_group: Option<i32>,
}

/// How the harness process ended, from [`spawn_and_wait`]'s point of view.
pub(crate) enum SpawnEnding {
    /// It ran to completion (successfully or not — that is `run.failure`'s
    /// business, not this type's).
    Finished(FinishedRun),
    /// The time limit ran out and the process was killed.
    TimedOut,
}

/// Runs one harness process outside any action attempt — no conversation
/// file, no `ORCHESTRATOR_ACTION_ID` — for the conductor (U9), which reuses
/// the spawning (through [`spawn_retrying_text_busy`]), the pipe draining and
/// the stream parsing of an agent step without being one. The elements the
/// parser returns are discarded with the lines: there is nowhere to record
/// them. So are the usage limits: the conductor has no reporter.
pub(crate) async fn run_harness_process(
    program: &str,
    args: &[String],
    working_dir: &Path,
    limit: Option<Duration>,
    parser: Box<dyn HarnessStreamParser>,
) -> Result<SpawnEnding, ActionError> {
    run_harness_invocation(
        program,
        args,
        None,
        working_dir,
        None,
        limit,
        FINAL_ANSWER_GRACE,
        parser,
        &ConversationSink::none(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_harness_invocation(
    program: &str,
    args: &[String],
    stdin_content: Option<&str>,
    working_dir: &Path,
    action_execution_id: Option<&str>,
    limit: Option<Duration>,
    grace: Duration,
    parser: Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
) -> Result<SpawnEnding, ActionError> {
    run_harness_invocation_with_sdk(
        program,
        args,
        stdin_content,
        working_dir,
        action_execution_id,
        limit,
        grace,
        parser,
        conversation,
        usage,
        &GitHubCopilotSdkRuntime::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_harness_invocation_with_sdk(
    program: &str,
    args: &[String],
    stdin_content: Option<&str>,
    working_dir: &Path,
    action_execution_id: Option<&str>,
    limit: Option<Duration>,
    grace: Duration,
    parser: Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
    sdk_runtime: &dyn CopilotSdkBackend,
) -> Result<SpawnEnding, ActionError> {
    if program == super::super::harness::github_copilot::DEFAULT_PROGRAM
        && args.first().is_some_and(|arg| arg == "agent")
    {
        return run_copilot_sdk(
            args,
            working_dir,
            action_execution_id,
            limit,
            parser,
            conversation,
            usage,
            sdk_runtime,
        )
        .await;
    }

    spawn_and_wait(
        program,
        args,
        stdin_content,
        working_dir,
        action_execution_id,
        limit,
        grace,
        parser,
        conversation,
        usage,
    )
    .await
}

/// How long the harness may go on after its final answer before its process
/// group is ended. Long enough for it to write its session to disk, so the
/// session can be resumed.
pub(crate) const FINAL_ANSWER_GRACE: Duration = Duration::from_secs(15);

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
/// conversation store — or, for a usage report, its limits to `usage`
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
/// final answer the step does not wait for the process to exit: after `grace`
/// the group is sent `SIGTERM` and then `SIGKILL`. The group is cleaned up
/// even when the harness exits by itself — a background task it left behind
/// keeps the stdout pipe open, so the pipe would never reach end of file.
#[allow(clippy::too_many_arguments)]
async fn spawn_and_wait(
    program: &str,
    args: &[String],
    stdin_content: Option<&str>,
    working_dir: &Path,
    action_execution_id: Option<&str>,
    limit: Option<Duration>,
    grace: Duration,
    parser: Box<dyn HarnessStreamParser>,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
) -> Result<SpawnEnding, ActionError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(working_dir);
    if let Some(action_execution_id) = action_execution_id {
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

    if let Some(content) = stdin_content {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(content.as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

    let mut parser = parser;
    let mut stderr_capture: Vec<u8> = Vec::new();
    // Kept here, not in the stdout task: the sender must outlive the task, or
    // the driving loop would see a closed channel where it expects an answer.
    let (answered_tx, mut answered_rx) = tokio::sync::watch::channel(false);

    let stdout_task = stream_lines(&mut stdout_pipe, |line| {
        let elements = if line.len() <= PARSE_LINE_LIMIT {
            parser.on_stdout_line(line)
        } else {
            vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::TooLarge,
            }]
        };
        record_stdout_line(parser.as_mut(), conversation, usage, line, &elements);
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
        conversation.append(ConversationSource::Stderr, line, &[stderr_element(line)]);
        accumulate_capped(&mut stderr_capture, line, MAX_BUFFERED_OUTPUT_BYTES);
        accumulate_capped(&mut stderr_capture, b"\n", MAX_BUFFERED_OUTPUT_BYTES);
    });

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

    let driven = match limit {
        Some(limit) => tokio::time::timeout(limit, drive).await,
        None => Ok(drive.await),
    };

    let driven = match driven {
        Ok(driven) => driven,
        Err(_elapsed) => {
            // The waiting future is gone, so the child can be reaped for
            // good. The still armed guard kills the rest of the group.
            let _ = child.kill().await;
            return Ok(SpawnEnding::TimedOut);
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

    Ok(SpawnEnding::Finished(FinishedRun {
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
/// A real tool-result line runs to single-digit megabytes at most (R14's own
/// justification cites "wielomegabajtowe linie wyników narzędzi" — a few
/// tens of MiB); a harness that streams for an entire timeout window with no
/// newline at all is not a line at any size R14 was written for, and letting
/// this accumulator grow without bound turns a misbehaving subprocess into
/// an OOM instead of the ordinary "this action failed" outcome every other
/// harness failure produces. 256 MiB is generous headroom above any
/// realistic single line while still being a bound.
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
            // raw OS error — otherwise both look identical to an operator,
            // even though one means "genuinely, persistently busy" and the
            // other never should have reached this branch at all.
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

/// The harness itself reported a failure.
pub(super) fn harness_error(
    adapter: &dyn HarnessAdapter,
    failure: &str,
    finished: &FinishedRun,
    run: &HarnessRun,
    resumed_session: Option<&str>,
) -> ActionError {
    ActionError {
        message: format!("the agent harness '{}' failed: {failure}", adapter.name()),
        error_type: Some(
            if resumed_session.is_some() {
                RESUME_SESSION_REJECTED
            } else {
                "harness_failed"
            }
            .to_string(),
        ),
        details: Some(serde_json::json!({
            "harness": adapter.name(),
            "program": adapter.program(),
            "exit_code": finished.exit_code,
            "session_id": run.session_id,
            "stderr": truncate(&finished.stderr),
            "resumed_session": resumed_session,
        })),
    }
}
