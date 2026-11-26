//! The problem log: what went wrong while the interface ran, kept in memory
//! for the operator to write to a file with `D`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

use super::background::{JobKey, Outcome};
use super::notify::Severity;
use super::{Focus, TaskApp, View};
use crate::api::CallFailure;
use crate::tui::keymap::Context;

/// How long an entry is kept, from the time it was recorded.
pub const WINDOW: Duration = Duration::from_secs(30 * 60);

/// The most entries kept; past it the oldest goes.
pub const MAX_ENTRIES: usize = 2000;

pub const MAX_DETAIL: usize = 4 * 1024;

/// The label of a call made outside any job or operator action (the event
/// stream).
pub const OUTSIDE_A_SCREEN: &str = "outside a screen";

tokio::task_local! {
    static OPERATION: String;
}

/// Runs `work` with `label` as the operation every failed call inside it is
/// recorded under.
pub async fn labeled<F: Future>(label: String, work: F) -> F::Output {
    OPERATION.scope(label, work).await
}

/// The operation the current task runs, or [`OUTSIDE_A_SCREEN`].
pub fn current_label() -> String {
    label_or(|| OUTSIDE_A_SCREEN.to_string())
}

/// The operation the current task runs, or what `fallback` says.
pub fn label_or(fallback: impl FnOnce() -> String) -> String {
    OPERATION
        .try_with(String::clone)
        .unwrap_or_else(|_| fallback())
}

/// The label of what `place` (a tab, a view) shows of `task_id`.
pub fn task_label(place: &str, task_id: &str) -> String {
    format!("{place} — task {task_id}")
}

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CallFailed,
    Shown,
}

impl Source {
    pub fn word(self) -> &'static str {
        match self {
            Self::CallFailed => "call failed",
            Self::Shown => "shown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub wall: DateTime<Local>,
    pub at: Instant,
    pub source: Source,
    pub label: String,
    pub call: Option<String>,
    /// The kind of the error of a failed call, or the severity of a shown
    /// text.
    pub kind: String,
    /// What the client knew of the failure, or the text shown; at most
    /// [`MAX_DETAIL`] bytes.
    pub detail: String,
    pub duration: Option<Duration>,
}

impl Problem {
    /// A failed call, as the client's observer told it, recorded at `at`.
    pub fn call_failed(label: String, failure: &CallFailure, at: Instant) -> Self {
        Self {
            wall: Local::now(),
            at,
            source: Source::CallFailed,
            label,
            call: Some(format!("{} {}", failure.method, failure.path)),
            kind: failure.kind.to_string(),
            detail: cut(&failure.detail),
            duration: Some(failure.elapsed),
        }
    }

    /// A text shown with `severity` (`error`, `warning`), recorded at `at`.
    pub fn shown(label: String, severity: &str, text: &str, at: Instant) -> Self {
        Self {
            wall: Local::now(),
            at,
            source: Source::Shown,
            label,
            call: None,
            kind: severity.to_string(),
            detail: cut(text),
            duration: None,
        }
    }
}

/// `text` cut to at most [`MAX_DETAIL`] bytes on a character boundary, with
/// `…` where it was cut.
fn cut(text: &str) -> String {
    if text.len() <= MAX_DETAIL {
        return text.to_string();
    }
    let marker = '…';
    let mut end = MAX_DETAIL - marker.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = text[..end].to_string();
    cut.push(marker);
    cut
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: Vec<Problem>,
    /// How many entries younger than [`WINDOW`] the size bound dropped since
    /// the interface started.
    pub dropped: usize,
}

#[derive(Debug, Default)]
struct Log {
    entries: VecDeque<Problem>,
    dropped: usize,
}

impl Log {
    fn prune(&mut self, now: Instant) {
        while self
            .entries
            .front()
            .is_some_and(|oldest| now.saturating_duration_since(oldest.at) > WINDOW)
        {
            self.entries.pop_front();
        }
    }
}

/// The log, shared by the client's observer and the application. Cheap to
/// clone; the lock is held only to push or to copy.
#[derive(Debug, Clone, Default)]
pub struct ProblemLog(Arc<Mutex<Log>>);

impl ProblemLog {
    /// Records `problem`, dropping what is older than [`WINDOW`] at `now`
    /// and the oldest entry past [`MAX_ENTRIES`].
    pub fn record(&self, problem: Problem, now: Instant) {
        let mut log = self.lock();
        log.prune(now);
        log.entries.push_back(problem);
        while log.entries.len() > MAX_ENTRIES {
            log.entries.pop_front();
            log.dropped += 1;
        }
    }

    /// What the log holds at `now`.
    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let mut log = self.lock();
        log.prune(now);
        Snapshot {
            entries: log.entries.iter().cloned().collect(),
            dropped: log.dropped,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Log> {
        // A panic while the lock was held left plain data behind: still
        // worth reading.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the head of a dump says about the interface that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: String,
    pub connection: String,
    pub timeout: Duration,
}

/// `url` with any user-info, query and fragment removed.
pub fn connection_of(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}/{path}")
        .trim_end_matches('/')
        .to_string()
}

/// The dump of `snapshot` as plain text, written at `written_at`.
pub fn render(snapshot: &Snapshot, header: &Header, written_at: DateTime<Local>) -> String {
    let mut text = String::new();
    let from = written_at - chrono::Duration::from_std(WINDOW).unwrap_or_default();
    text.push_str("Narsor problem log\n");
    let _ = writeln!(text, "version:      {}", header.version);
    let _ = writeln!(text, "connection:   {}", header.connection);
    let _ = writeln!(text, "timeout:      {} ms", header.timeout.as_millis());
    let _ = writeln!(
        text,
        "written at:   {}",
        written_at.format("%Y-%m-%d %H:%M:%S %:z")
    );
    let _ = write!(
        text,
        "covers:       {} – {} ({} min); {} entries; {} dropped by the cap\n\n",
        from.format("%H:%M:%S"),
        written_at.format("%H:%M:%S"),
        WINDOW.as_secs() / 60,
        snapshot.entries.len(),
        snapshot.dropped,
    );
    if snapshot.entries.is_empty() {
        let _ = writeln!(
            text,
            "no problems recorded in the last {} minutes",
            WINDOW.as_secs() / 60
        );
        return text;
    }
    const INDENT: &str = "              ";
    for problem in &snapshot.entries {
        let _ = write!(
            text,
            "{}  {:<12}  {}",
            problem.wall.format("%H:%M:%S%.3f"),
            problem.source.word(),
            problem.label,
        );
        if let Some(call) = &problem.call {
            let _ = write!(text, "   {call}");
        }
        text.push('\n');
        match problem.source {
            Source::CallFailed => {
                let _ = write!(text, "{INDENT}kind: {}", problem.kind);
                if let Some(duration) = problem.duration {
                    let _ = write!(text, "   after: {} ms", duration.as_millis());
                }
                text.push('\n');
                let _ = writeln!(text, "{INDENT}detail: {}", one_line_raw(&problem.detail));
            }
            Source::Shown => {
                let _ = writeln!(
                    text,
                    "{INDENT}{}: {}",
                    problem.kind,
                    one_line_raw(&problem.detail)
                );
            }
        }
    }
    text
}

fn one_line_raw(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// The name of a dump written at `at`: `narsor-problems-<date>-<time>-<pid>`,
/// with `-2`, `-3`, … after a name that is taken.
fn dump_name(at: DateTime<Local>, attempt: u32) -> String {
    let stamp = at.format("%Y%m%d-%H%M%S");
    let pid = std::process::id();
    if attempt <= 1 {
        format!("narsor-problems-{stamp}-{pid}.txt")
    } else {
        format!("narsor-problems-{stamp}-{pid}-{attempt}.txt")
    }
}

/// Writes `text` to a new file in `dir`, readable by its owner only, and
/// returns its path. An existing file is never written over.
pub fn write_dump(dir: &Path, text: &str, at: DateTime<Local>) -> io::Result<PathBuf> {
    const ATTEMPTS: u32 = 100;
    for attempt in 1..=ATTEMPTS {
        let path = dir.join(dump_name(at, attempt));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{ATTEMPTS} file names in {} are taken", dir.display()),
    ))
}

impl TaskApp {
    /// Writes the dumps of `D` to `dir` in place of the system's temporary
    /// directory: what a test sets.
    #[must_use]
    pub fn with_dump_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dump_dir = dir.into();
        self
    }

    /// Whether a typed key is text now: the list filter, or a field of a
    /// form, the panel or the conductor's chat. There `D` is a letter.
    pub fn accepts_text(&self) -> bool {
        let filtering = matches!(self.view, View::Tasks)
            && self.focus == Focus::List
            && self.list.is_filtering();
        filtering
            || matches!(
                self.key_context().base,
                Context::Form | Context::FormConfirm
            )
    }

    /// `D`: writes what the log holds now to a new file, in the background.
    /// The bar shows the file's path, or why it could not be written; the
    /// log stays as it was either way. A second `D` while one is written
    /// does nothing.
    pub(super) fn start_dump(&mut self) {
        let now = Instant::now();
        let written_at = Local::now();
        let header = Header {
            version: narsor_protocol::VERSION.to_string(),
            connection: connection_of(self.client.base_url()),
            timeout: self.client.request_timeout(),
        };
        let text = render(&self.problems.snapshot(now), &header, written_at);
        let dir = self.dump_dir.clone();
        self.background.start(JobKey::Dump, async move {
            let written =
                tokio::task::spawn_blocking(move || write_dump(&dir, &text, written_at)).await;
            let written = written.unwrap_or_else(|error| Err(io::Error::other(error)));
            Outcome::new(move |app: &mut TaskApp| app.on_dump_written(written))
        });
    }

    fn on_dump_written(&mut self, written: io::Result<PathBuf>) {
        let now = Instant::now();
        match written {
            Ok(path) => self.notifier.push_held(
                Severity::Success,
                format!("dziennik problemów zapisany: {}", path.display()),
                now,
            ),
            Err(error) => self.notifier.push_held(
                Severity::Error,
                format!("nie udało się zapisać dziennika problemów: {error}"),
                now,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown_at(at: Instant, text: &str) -> Problem {
        Problem::shown("Przebieg — task t1".to_string(), "error", text, at)
    }

    #[test]
    fn entries_older_than_thirty_minutes_are_gone_from_the_snapshot() {
        let base = Instant::now();
        let log = ProblemLog::default();
        log.record(shown_at(base, "old"), base);
        let recent = base + Duration::from_secs(26 * 60);
        log.record(shown_at(recent, "recent"), recent);

        let snapshot = log.snapshot(base + Duration::from_secs(31 * 60));

        let texts: Vec<_> = snapshot.entries.iter().map(|p| p.detail.as_str()).collect();
        assert_eq!(texts, ["recent"]);
    }

    #[test]
    fn past_the_cap_the_oldest_entry_goes_and_is_counted() {
        let base = Instant::now();
        let log = ProblemLog::default();
        for index in 0..=MAX_ENTRIES {
            let at = base + Duration::from_millis(index as u64 * 20);
            log.record(shown_at(at, &format!("problem {index}")), at);
        }

        let snapshot = log.snapshot(base + Duration::from_secs(60));

        assert_eq!(snapshot.entries.len(), MAX_ENTRIES);
        assert_eq!(snapshot.dropped, 1);
        assert_eq!(snapshot.entries[0].detail, "problem 1");
        assert_eq!(
            snapshot.entries.last().unwrap().detail,
            format!("problem {MAX_ENTRIES}")
        );
    }

    #[test]
    fn repeated_problems_are_all_kept() {
        let base = Instant::now();
        let log = ProblemLog::default();
        for step in 0..3 {
            let at = base + Duration::from_secs(step);
            log.record(shown_at(at, "same text"), at);
        }

        let snapshot = log.snapshot(base + Duration::from_secs(10));

        assert_eq!(snapshot.entries.len(), 3);
    }

    #[test]
    fn a_long_detail_is_cut_on_a_character_boundary() {
        let long = "żółć".repeat(10 * 1024 / "żółć".len() + 1);
        let problem = shown_at(Instant::now(), &long);
        assert!(
            problem.detail.len() <= MAX_DETAIL,
            "{}",
            problem.detail.len()
        );
        assert!(problem.detail.ends_with('…'));
        assert!(long.starts_with(problem.detail.trim_end_matches('…')));
    }
}
