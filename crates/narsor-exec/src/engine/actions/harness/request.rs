//! What a harness is asked to run, what it says while it runs, and how the
//! run ended: the only input and output of [`super::HarnessAdapter::run`]
//! (R1–R4).
//!
//! The request is harness-neutral. Every caller — the agent step, the
//! profile step, the conductor — fills the same fields, and each harness
//! decides alone how they reach its CLI or SDK: which flags, whether the
//! prompt goes on the command line or on stdin, how the context files are
//! made readable. No caller reads a command line back.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::HarnessRun;
use crate::domain::{ActionError, WorkspaceAccess};
use crate::engine::actions::answer::AnswerShape;
use crate::engine::actions::session::conversation::{ConversationSink, UsageRelay};

/// How long the harness may go on after its final answer before its process
/// group is ended. Long enough for it to write its session to disk, so the
/// session can be resumed.
pub const FINAL_ANSWER_GRACE: Duration = Duration::from_secs(15);

/// One run of an agent, as every harness takes it.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The prompt as the agent is to read it, output contract included. Of
    /// any size: the harness picks how it travels (R5).
    pub prompt: String,

    /// Files the agent may read when it needs them, kept out of the prompt.
    pub context: Option<ContextFiles>,

    /// The model tier (`small`, `medium`, `large`, `max`); `None` leaves the
    /// model to the harness.
    pub model: Option<String>,

    /// The effort, for a harness that takes one.
    pub effort: Option<String>,

    /// A harness session to continue instead of starting a fresh one (D18).
    pub resume_session: Option<String>,

    /// The directory the harness runs in.
    pub working_dir: PathBuf,

    /// What the agent may do in its workspace, translated by each harness
    /// to its own permission mechanism.
    pub workspace_access: Option<WorkspaceAccess>,

    /// Repositories the run may change, by absolute path.
    pub writable_dirs: Vec<PathBuf>,

    /// Repositories the run may only read, by absolute path. Harnesses that
    /// cannot enforce it rely on the prompt block (R14).
    pub read_only_dirs: Vec<PathBuf>,

    /// The step's `read_only_paths`, relative to `working_dir` or absolute.
    pub read_only_paths: Vec<String>,

    /// One-shot tool permissions an operator granted (Claude Code rules).
    pub permission_grants: Vec<String>,

    /// The step's harness-specific `parameters` (`max_turns`,
    /// `append_system_prompt`, `agent`, …); each harness checks its own.
    pub parameters: HashMap<String, Value>,

    /// The JSON Schema of the answer, for a harness that can enforce one.
    /// An addition to the text contract in `prompt`, never a replacement.
    pub answer_schema: Option<Value>,

    /// What the final answer looks like, which ends the run early.
    pub answer_shape: AnswerShape,

    /// The time limit of the whole run; `None` is unbounded.
    pub timeout: Option<Duration>,

    /// How long the harness may go on after its final answer.
    pub final_answer_grace: Duration,

    /// The action attempt the run belongs to, handed to the harness process
    /// as `ORCHESTRATOR_ACTION_ID`; `None` outside an attempt (the
    /// conductor).
    pub action_execution_id: Option<String>,

    /// The operator's extra arguments for this harness; on a conflict they
    /// replace the flags the harness chose.
    pub extra_args: Vec<String>,
}

impl AgentRequest {
    /// A request for `prompt` in `working_dir`, everything else unset.
    pub fn new(prompt: impl Into<String>, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            context: None,
            model: None,
            effort: None,
            resume_session: None,
            working_dir: working_dir.into(),
            workspace_access: None,
            writable_dirs: Vec::new(),
            read_only_dirs: Vec::new(),
            read_only_paths: Vec::new(),
            permission_grants: Vec::new(),
            parameters: HashMap::new(),
            answer_schema: None,
            answer_shape: AnswerShape::default(),
            timeout: None,
            final_answer_grace: FINAL_ANSWER_GRACE,
            action_execution_id: None,
            extra_args: Vec::new(),
        }
    }

    /// Those of `dirs` that lie outside the working directory.
    pub(crate) fn outside_working_dir<'d>(&self, dirs: &'d [PathBuf]) -> Vec<&'d Path> {
        dirs.iter()
            .map(PathBuf::as_path)
            .filter(|dir| !dir.starts_with(&self.working_dir))
            .collect()
    }
}

/// One file of context: a name relative to the context directory, what it
/// holds in one line, and its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// The path inside the context directory, e.g. `conversations/plan-1.md`.
    pub name: String,
    /// What the file holds, in one line, for the prompt's list.
    pub description: String,
    /// The whole content, uncut.
    pub content: ContextContent,
}

impl ContextFile {
    /// A file named `name`, described by `description`.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        content: impl Into<ContextContent>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            content: content.into(),
        }
    }
}

/// Where a context file's content comes from. A large content that already
/// lives somewhere (a conversation record, an artifact) is a source: it is
/// written straight to the file, so it is never held whole in memory next
/// to every other file.
#[derive(Clone)]
pub enum ContextContent {
    /// Text in memory.
    Text(Arc<str>),
    /// Content written to the file when the context is prepared.
    Source(Arc<dyn ContextSource>),
}

/// Content a context file copies from somewhere else when it is written.
pub trait ContextSource: Send + Sync + fmt::Debug {
    /// Writes the whole content to `out`; nothing written means no file.
    fn write_to(&self, out: &mut dyn io::Write) -> io::Result<()>;
}

impl ContextContent {
    /// The whole content as text, read from the source when it is one.
    pub fn read_to_string(&self) -> io::Result<String> {
        match self {
            Self::Text(text) => Ok(text.to_string()),
            Self::Source(source) => {
                let mut bytes = Vec::new();
                source.write_to(&mut bytes)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
        }
    }
}

impl<T: Into<String>> From<T> for ContextContent {
    fn from(text: T) -> Self {
        Self::Text(Arc::from(text.into()))
    }
}

impl fmt::Debug for ContextContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(&text.len()).finish(),
            Self::Source(source) => f.debug_tuple("Source").field(source).finish(),
        }
    }
}

impl PartialEq for ContextContent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Source(a), Self::Source(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for ContextContent {}

/// The context files of one run and the directory they are written to. The
/// directory belongs to the run: whatever it held before is removed (R13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFiles {
    /// Where the files are written; emptied first.
    pub dir: PathBuf,
    /// The files, in the order the prompt lists them. A file without
    /// content is neither written nor listed.
    pub files: Vec<ContextFile>,
}

/// The heading of the block a harness appends to the prompt.
const CONTEXT_FILES_HEADING: &str = "## Context files";

impl ContextFiles {
    /// Replaces the directory's content with the files and returns the
    /// block that lists them for the agent; an empty string when there is
    /// no file to list. Every stock harness can read the directory (R6).
    fn prepare(&self) -> Result<String, ActionError> {
        for file in &self.files {
            check_name(&file.name)?;
        }
        remove_dir_if_present(&self.dir)
            .and_then(|()| create_private_dir(&self.dir))
            .map_err(|error| context_io_error(&self.dir, "emptied", &error))?;

        let mut listed = String::new();
        for file in &self.files {
            if matches!(&file.content, ContextContent::Text(text) if text.is_empty()) {
                continue;
            }
            let path = self.dir.join(&file.name);
            let bytes = write_file(&path, &file.content)?;
            if bytes == 0 {
                // A source with nothing to copy has no file, like empty text.
                fs::remove_file(&path)
                    .map_err(|error| context_io_error(&path, "removed", &error))?;
                continue;
            }
            let _ = writeln!(
                listed,
                "- `{}` ({bytes} bytes): {}",
                path.display(),
                file.description
            );
        }
        if listed.is_empty() {
            return Ok(String::new());
        }
        Ok(format!(
            "{CONTEXT_FILES_HEADING}\n\nThese files hold more context for this run. They are \
             not part of the prompt: read a file only when you need what it holds.\n\n{listed}"
        ))
    }
}

impl AgentRequest {
    /// `prompt` as the agent receives it: the context files written, and
    /// the block listing them appended after a blank line.
    ///
    /// The files are written on a blocking thread: a source can copy a
    /// large record, which must not hold up the async runtime.
    pub(crate) async fn prompt_with_context(
        &self,
        mut prompt: String,
    ) -> Result<String, ActionError> {
        let Some(context) = self.context.clone() else {
            return Ok(prompt);
        };
        let dir = context.dir.clone();
        let block = tokio::task::spawn_blocking(move || context.prepare())
            .await
            .map_err(|error| ActionError {
                message: format!(
                    "the context files at {} could not be written: {error}",
                    dir.display()
                ),
                error_type: Some("context_files_failed".to_string()),
                details: Some(serde_json::json!({ "path": dir.display().to_string() })),
            })??;
        if !block.is_empty() {
            prompt.truncate(prompt.trim_end().len());
            prompt.push_str("\n\n");
            prompt.push_str(block.trim_end());
        }
        Ok(prompt)
    }
}

/// Empties `dir`, creating it when it does not exist.
pub(crate) fn empty_dir(dir: &Path) -> std::io::Result<()> {
    remove_dir_if_present(dir)?;
    fs::create_dir_all(dir)
}

fn remove_dir_if_present(dir: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Context files carry conversation records, the same sensitivity class
/// the conversation store keeps owner-only: directories are created 0700
/// and files 0600 on unix.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Writes `content` to `path` and returns how many bytes it holds.
fn write_private_file(path: &Path, content: &ContextContent) -> io::Result<u64> {
    use std::io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = CountingWriter {
        inner: io::BufWriter::new(options.open(path)?),
        written: 0,
    };
    match content {
        ContextContent::Text(text) => out.write_all(text.as_bytes())?,
        ContextContent::Source(source) => source.write_to(&mut out)?,
    }
    out.flush()?;
    Ok(out.written)
}

/// A writer that counts the bytes it passes on.
struct CountingWriter<W> {
    inner: W,
    written: u64,
}

impl<W: io::Write> io::Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A context file name must stay inside its directory.
fn check_name(name: &str) -> Result<(), ActionError> {
    let path = Path::new(name);
    let inside = !name.is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if inside {
        return Ok(());
    }
    Err(ActionError {
        message: format!("the context file name '{name}' leaves its directory"),
        error_type: Some("invalid_context_file".to_string()),
        details: Some(serde_json::json!({ "name": name })),
    })
}

fn write_file(path: &Path, content: &ContextContent) -> Result<u64, ActionError> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent).map_err(|error| context_io_error(parent, "created", &error))?;
    }
    write_private_file(path, content).map_err(|error| context_io_error(path, "written", &error))
}

fn context_io_error(path: &Path, what: &str, error: &std::io::Error) -> ActionError {
    ActionError {
        message: format!(
            "the context files at {} could not be {what}: {error}",
            path.display()
        ),
        error_type: Some("context_files_failed".to_string()),
        details: Some(serde_json::json!({ "path": path.display().to_string() })),
    }
}

/// The model and the effort a harness resolved for a run, told before the
/// run starts (R4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunStarted {
    /// The model name as the harness takes it, not the tier.
    pub model: Option<String>,
    /// The effort as the harness takes it.
    pub effort: Option<String>,
}

/// Where a run reports while it goes (R3): every stdout line with its
/// conversation elements (and the tokens and cost they report), the usage
/// limits, and the resolved model and effort once the run is about to start.
pub struct RunObservers<'a> {
    pub(crate) conversation: &'a ConversationSink,
    pub(crate) usage: Option<UsageRelay<'a>>,
    pub(crate) started: Option<&'a (dyn Fn(&RunStarted) + Send + Sync)>,
}

static NO_CONVERSATION: ConversationSink = ConversationSink::none();

impl<'a> RunObservers<'a> {
    /// Observers that record nothing.
    pub fn none() -> Self {
        Self {
            conversation: &NO_CONVERSATION,
            usage: None,
            started: None,
        }
    }

    /// Records the conversation in `conversation`.
    pub(crate) fn conversation(conversation: &'a ConversationSink) -> Self {
        Self {
            conversation,
            ..Self::none()
        }
    }

    /// Hands the usage limits to `usage`.
    pub(crate) fn with_usage(mut self, usage: Option<UsageRelay<'a>>) -> Self {
        self.usage = usage;
        self
    }

    /// Calls `started` once the harness has resolved the model and the
    /// effort, right before it starts.
    pub fn on_started(mut self, started: &'a (dyn Fn(&RunStarted) + Send + Sync)) -> Self {
        self.started = Some(started);
        self
    }

    /// Tells the `started` observer, if there is one.
    pub(crate) fn notify_started(&self, started: RunStarted) {
        if let Some(observer) = self.started {
            observer(&started);
        }
    }
}

/// A harness run that ran to completion, already parsed.
#[derive(Debug)]
pub struct FinishedRun {
    /// What the harness's stream parser distilled from its output.
    pub run: HarnessRun,
    /// The process's exit code (0 for an SDK run).
    pub exit_code: i32,
    /// A bounded capture of stderr, for error messages only — the full
    /// text, unbounded, is what the conversation holds (R14).
    pub stderr: String,
    /// The process group that was still alive after the final answer and
    /// was ended by narsor, when there was one.
    pub terminated_group: Option<i32>,
}

/// How a run ended.
// One value per run, moved once: boxing the large variant buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum AgentRunEnding {
    /// It ran to completion (successfully or not — that is `run.failure`'s
    /// business, not this type's).
    Finished(FinishedRun),
    /// The time limit ran out and the run was stopped.
    TimedOut,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh directory, removed when dropped.
    pub(crate) struct TestDir(PathBuf);

    impl TestDir {
        pub(crate) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("narsor-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn context(dir: &Path, files: &[(&str, &str)]) -> ContextFiles {
        ContextFiles {
            dir: dir.to_path_buf(),
            files: files
                .iter()
                .map(|(name, content)| ContextFile::new(*name, format!("about {name}"), *content))
                .collect(),
        }
    }

    #[test]
    fn readable_files_are_written_and_listed_by_path_description_and_size() {
        let root = TestDir::new();
        let dir = root.path().join("context");
        let block = context(
            &dir,
            &[("events.md", "abc"), ("conversations/a-1.md", "hello")],
        )
        .prepare()
        .unwrap();

        assert_eq!(fs::read_to_string(dir.join("events.md")).unwrap(), "abc");
        assert_eq!(
            fs::read_to_string(dir.join("conversations/a-1.md")).unwrap(),
            "hello"
        );
        let events = dir.join("events.md");
        assert!(
            block.contains(&format!(
                "- `{}` (3 bytes): about events.md",
                events.display()
            )),
            "{block}"
        );
        assert!(
            block.contains("(5 bytes): about conversations/a-1.md"),
            "{block}"
        );
    }

    #[test]
    fn a_new_run_leaves_only_its_own_files_and_lists_no_empty_one() {
        let root = TestDir::new();
        let dir = root.path().join("context");
        context(&dir, &[("old.md", "old")])
            .prepare()
            .unwrap();

        let block = context(&dir, &[("new.md", "new"), ("empty.md", "")])
            .prepare()
            .unwrap();

        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["new.md"]);
        assert!(!block.contains("empty.md"), "{block}");

        let block = context(&dir, &[]).prepare().unwrap();
        assert!(block.is_empty());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn a_name_that_leaves_the_directory_is_refused() {
        let root = TestDir::new();
        for name in ["../escape.md", "/etc/passwd", ""] {
            let error = context(&root.path().join("context"), &[(name, "x")])
                .prepare()
                .expect_err(name);
            assert_eq!(error.error_type.as_deref(), Some("invalid_context_file"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn context_files_are_readable_by_their_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new();
        let dir = root.path().join("context");
        context(&dir, &[("conversations/a-1.md", "secret")])
            .prepare()
            .unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("conversations")), 0o700);
        assert_eq!(mode(&dir.join("conversations/a-1.md")), 0o600);
    }
}
