//! The problem log: what went wrong while the interface ran, kept in memory
//! for the operator to write to a file with `D`.
//!
//! The screens say "brak połączenia z serwerem" for every transport failure,
//! and the trace log (`narsor.log`) is only written when a log level was
//! configured beforehand. This log is always on and costs nothing on disk:
//! every failed call to the server (with the cause the client saw and how
//! long the call took) and every error or warning the interface showed, for
//! the last [`WINDOW`]. Nothing is written until the operator asks for it.
//!
//! * A failed call is told by the client's failure observer
//!   ([`crate::api::ApiClient::with_failure_observer`]), labelled with the
//!   operation that made it ([`labeled`], [`current_label`]): the job or the
//!   operator action awaiting the call sets the label around it.
//! * What was shown is recorded where errors become words: the notification
//!   on the bar and the failure text of a tab.
//! * The log is bounded by age ([`WINDOW`]) and by size ([`MAX_ENTRIES`],
//!   [`MAX_DETAIL`]); repeats are kept, because when failures happen is the
//!   evidence.
//! * The time is handed in by the caller, as for the notifications, so a
//!   test steps it without sleeping.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};

use crate::api::CallFailure;

/// How long an entry is kept, from the time it was recorded.
pub const WINDOW: Duration = Duration::from_secs(30 * 60);

/// The most entries kept; past it the oldest goes.
pub const MAX_ENTRIES: usize = 2000;

/// The most bytes of one entry's detail.
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
    OPERATION.try_with(String::clone).unwrap_or_else(|_| fallback())
}

/// The label of what `place` (a tab, a view) shows of `task_id`.
pub fn task_label(place: &str, task_id: &str) -> String {
    format!("{place} — task {task_id}")
}

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A call to the server failed.
    CallFailed,
    /// The interface showed an error or a warning.
    Shown,
}

impl Source {
    /// The word the dump uses.
    pub fn word(self) -> &'static str {
        match self {
            Self::CallFailed => "call failed",
            Self::Shown => "shown",
        }
    }
}

/// One problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// When it was recorded, on the wall clock.
    pub wall: DateTime<Local>,
    /// When it was recorded, for its age.
    pub at: Instant,
    /// A failed call or a shown text.
    pub source: Source,
    /// The screen or tab and the operation.
    pub label: String,
    /// The method and path of a failed call.
    pub call: Option<String>,
    /// The kind of the error of a failed call, or the severity of a shown
    /// text.
    pub kind: String,
    /// What the client knew of the failure, or the text shown; at most
    /// [`MAX_DETAIL`] bytes.
    pub detail: String,
    /// How long a failed call took.
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

/// What the log holds at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The entries younger than [`WINDOW`], oldest first.
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

// ---------------------------------------------------------------------
// The dump
// ---------------------------------------------------------------------

/// What the head of a dump says about the interface that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The version of narsor.
    pub version: String,
    /// The server, as a URL with no user-info and no query.
    pub connection: String,
    /// The bound of an ordinary request.
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
    text.push_str(&format!("version:      {}\n", header.version));
    text.push_str(&format!("connection:   {}\n", header.connection));
    text.push_str(&format!(
        "timeout:      {} ms\n",
        header.timeout.as_millis()
    ));
    text.push_str(&format!(
        "written at:   {}\n",
        written_at.format("%Y-%m-%d %H:%M:%S %:z")
    ));
    text.push_str(&format!(
        "covers:       {} – {} ({} min); {} entries; {} dropped by the cap\n\n",
        from.format("%H:%M:%S"),
        written_at.format("%H:%M:%S"),
        WINDOW.as_secs() / 60,
        snapshot.entries.len(),
        snapshot.dropped,
    ));
    if snapshot.entries.is_empty() {
        text.push_str(&format!(
            "no problems recorded in the last {} minutes\n",
            WINDOW.as_secs() / 60
        ));
        return text;
    }
    const INDENT: &str = "              ";
    for problem in &snapshot.entries {
        text.push_str(&format!(
            "{}  {:<12}  {}",
            problem.wall.format("%H:%M:%S%.3f"),
            problem.source.word(),
            problem.label,
        ));
        if let Some(call) = &problem.call {
            text.push_str(&format!("   {call}"));
        }
        text.push('\n');
        match problem.source {
            Source::CallFailed => {
                text.push_str(&format!("{INDENT}kind: {}", problem.kind));
                if let Some(duration) = problem.duration {
                    text.push_str(&format!("   after: {} ms", duration.as_millis()));
                }
                text.push('\n');
                text.push_str(&format!(
                    "{INDENT}detail: {}\n",
                    one_line_raw(&problem.detail)
                ));
            }
            Source::Shown => text.push_str(&format!(
                "{INDENT}{}: {}\n",
                problem.kind,
                one_line_raw(&problem.detail)
            )),
        }
    }
    text
}

/// `text` on one line: a line break would break the layout of the dump.
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
    fn a_long_detail_is_cut_on_a_character_boundary() {
        let long = "żółć".repeat(10 * 1024 / "żółć".len() + 1);
        let problem = shown_at(Instant::now(), &long);
        assert!(problem.detail.len() <= MAX_DETAIL, "{}", problem.detail.len());
        assert!(problem.detail.ends_with('…'));
        assert!(long.starts_with(problem.detail.trim_end_matches('…')));
    }
}
