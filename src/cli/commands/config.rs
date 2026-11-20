//! `config show`: every setting of every binary installed next to
//! `narsor-cli`, its value and its source (R14, R15, R24).
//!
//! `narsor-cli` checks its own scope in process. Each other binary checks
//! its own part of the file when run with `--check-config`
//! ([`CheckReport`]); `config show` runs every one installed next to it,
//! through the [`ConfigChecker`] seam, and merges the reports by path: a
//! setting several binaries read with the same value is one row naming them
//! all.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::api::launch::sibling_executable;
use crate::cli::ConfigFormat;
use crate::config::check::{CheckedSetting, CheckedSource};
use crate::config::{
    CheckReport, Problem, Resolved, Scope, Section, SettingKey, CHECK_CONFIG_FLAG, MASK,
};

use super::{CliError, Result};

/// The name `narsor-cli`'s own report carries.
pub const CLI_BINARY: &str = "narsor-cli";

/// A binary `config show` asks for its check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sibling {
    /// Its executable name.
    pub name: &'static str,
    /// Whether it has a `--root` flag.
    pub takes_root: bool,
    /// The part of the file it reads.
    pub scope: Scope,
}

/// Every binary that reads the configuration file, beside `narsor-cli`.
pub const SIBLINGS: [Sibling; 5] = [
    Sibling {
        name: "narsor-server",
        takes_root: true,
        scope: Scope::SERVER,
    },
    Sibling {
        name: "narsor-web",
        takes_root: true,
        scope: Scope::WEB,
    },
    Sibling {
        name: "narsor",
        takes_root: true,
        scope: Scope::TUI,
    },
    Sibling {
        name: "narsor-runner",
        takes_root: false,
        scope: Scope::RUNNER,
    },
    Sibling {
        name: "narsor-controller",
        takes_root: false,
        scope: Scope::CONTROLLER,
    },
];

/// What asking one binary for its check gave.
#[derive(Debug, Clone)]
pub enum CheckOutcome {
    /// Its report.
    Report(CheckReport),
    /// It is not installed next to `narsor-cli`; not an error (R24).
    NotInstalled,
    /// It did not produce a report, and why.
    Failed(String),
}

/// Asks a sibling binary for its check: the seam tests feed canned reports
/// through.
pub trait ConfigChecker {
    /// The outcome of `sibling`'s check.
    fn check(&self, sibling: &Sibling) -> CheckOutcome;
}

/// How long one sibling's check may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The most a sibling's report may weigh.
const MAX_REPORT_BYTES: u64 = 1024 * 1024;

/// The most of a sibling's stderr kept for a failure message.
const MAX_STDERR_BYTES: u64 = 64 * 1024;

/// How often a running check is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The production checker: runs the sibling found in the directory of this
/// executable with `--check-config`, the file `narsor-cli` resolved and —
/// for a binary that has the flag — `--root`.
#[derive(Debug, Clone)]
pub struct ProcessChecker {
    config: Option<PathBuf>,
    root: Option<PathBuf>,
}

impl ProcessChecker {
    /// A checker passing `config` (the winning file) and `root` (the
    /// operator's `--root`).
    pub fn new(config: Option<PathBuf>, root: Option<PathBuf>) -> Self {
        Self { config, root }
    }
}

impl ConfigChecker for ProcessChecker {
    fn check(&self, sibling: &Sibling) -> CheckOutcome {
        let program = match sibling_executable(sibling.name) {
            Ok(Some(program)) => program,
            Ok(None) => return CheckOutcome::NotInstalled,
            Err(error) => return CheckOutcome::Failed(format!("cannot look for it: {error}")),
        };
        let mut command = Command::new(program);
        command.arg(CHECK_CONFIG_FLAG);
        if let Some(file) = &self.config {
            command.arg("--config").arg(file);
        }
        if let (true, Some(root)) = (sibling.takes_root, &self.root) {
            command.arg("--root").arg(root);
        }
        run_check(command)
    }
}

/// Runs one check — `command` is the sibling with its arguments — and
/// returns its report, or why there is none: an exit code other than 0 or
/// 1, output that is not a report, a run longer than [`CHECK_TIMEOUT`]
/// (killed) or a report over [`MAX_REPORT_BYTES`].
pub fn run_check(mut command: Command) -> CheckOutcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return CheckOutcome::Failed(format!("could not start: {error}")),
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    // Read on threads, so a full pipe cannot stall the child; a reader that
    // stops at its limit closes the pipe, which ends a child still writing.
    let stdout = std::thread::spawn(move || read_limited(stdout, MAX_REPORT_BYTES));
    let stderr = std::thread::spawn(move || read_limited(stderr, MAX_STDERR_BYTES));

    let deadline = Instant::now() + CHECK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "did not finish within {} s",
                    CHECK_TIMEOUT.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.to_string());
            }
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    let status = match status {
        Ok(status) => status,
        Err(reason) => return CheckOutcome::Failed(reason),
    };
    if stdout.len() as u64 > MAX_REPORT_BYTES {
        return CheckOutcome::Failed(format!("printed more than {MAX_REPORT_BYTES} bytes"));
    }
    let last_line = String::from_utf8_lossy(&stderr)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| format!(": {}", line.trim()))
        .unwrap_or_default();
    if !matches!(status.code(), Some(0 | 1)) {
        return CheckOutcome::Failed(format!("ended with {status}{last_line}"));
    }
    match CheckReport::from_json(&String::from_utf8_lossy(&stdout)) {
        Ok(report) => CheckOutcome::Report(report),
        Err(reason) => CheckOutcome::Failed(format!("{reason}{last_line}")),
    }
}

/// Up to `limit + 1` bytes of `reader`: one more than allowed tells the
/// caller the limit was passed.
fn read_limited(reader: impl Read, limit: u64) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.take(limit + 1).read_to_end(&mut buffer);
    buffer
}

/// `narsor config show [--format json]` — every setting of every installed
/// binary, its value, its source and the binaries that read it.
///
/// The complete set, including the settings nobody touched: a listing that
/// hides the built-ins answers "what did I change" when the question being
/// asked is "what is in force" (R7).
///
/// # It still works when the configuration does not
///
/// This is the only command that runs on a file it could not use (D-D). An
/// operator reaching for it is, by definition, reaching for it because
/// something is wrong. Broken keys are printed as rows carrying the
/// complaint, and the command exits 1 when any binary reported a problem or
/// could not be checked, so that a script branching on the code is not
/// misled. A binary that is not installed is listed and changes nothing.
///
/// The one failure it does not survive is a `--config` path that is not there
/// — that is a usage error, handled before this is ever called.
pub fn config_show(config: &Resolved, root: Option<&Path>, format: ConfigFormat) -> Result<()> {
    let own = CheckReport::build(CLI_BINARY, config, &Problem::of_resolution(config));
    let checker = ProcessChecker::new(
        config.file().path().map(Path::to_path_buf),
        root.map(Path::to_path_buf),
    );
    config_show_with(own, &checker, format, &mut std::io::stdout().lock())
}

/// [`config_show`] with `narsor-cli`'s own report and the checker of the
/// other binaries handed in, writing to `out`.
pub fn config_show_with(
    own: CheckReport,
    checker: &dyn ConfigChecker,
    format: ConfigFormat,
    out: &mut dyn Write,
) -> Result<()> {
    let mut checks = vec![BinaryCheck {
        name: CLI_BINARY,
        scope: Scope::CLI,
        outcome: CheckOutcome::Report(own),
    }];
    checks.extend(SIBLINGS.iter().map(|sibling| BinaryCheck {
        name: sibling.name,
        scope: sibling.scope,
        outcome: checker.check(sibling),
    }));
    let merged = Merged::new(&checks);

    let text = match format {
        ConfigFormat::Table => merged.table(),
        ConfigFormat::Json => merged.json(),
    };
    // A closed stdout is the reader's choice; the exit code still tells.
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();

    let failing: Vec<String> = merged
        .statuses
        .iter()
        .filter(|status| status.state.is_failure())
        .map(|status| status.name.to_string())
        .collect();
    if failing.is_empty() {
        return Ok(());
    }
    // The payload has already gone to stdout; this is the diagnostic half, and
    // it carries the exit code.
    Err(CliError::failure(format!(
        "The configuration does not hold up for {}; see above.",
        failing.join(", ")
    )))
}

/// One binary and what its check gave.
struct BinaryCheck {
    name: &'static str,
    scope: Scope,
    outcome: CheckOutcome,
}

/// One row: a path with one value, and the binaries that report it so.
struct Row<'a> {
    path: &'a str,
    setting: &'a CheckedSetting,
    binaries: Vec<&'static str>,
}

impl Row<'_> {
    /// The section the row belongs to: the first segment of its path.
    fn section(&self) -> &str {
        self.path.split('.').next().unwrap_or(self.path)
    }

    /// The value to print: masked when sensitive, whatever the report says.
    fn value(&self) -> Option<String> {
        if self.setting.sensitive {
            return Some(MASK.to_string());
        }
        self.setting.value.clone()
    }
}

/// A problem that marks no row, and the binaries that report it.
struct GeneralError<'a> {
    path: Option<&'a str>,
    message: &'a str,
    binaries: Vec<&'static str>,
}

/// How one binary's check went.
enum State {
    Ok,
    Errors(usize),
    NotInstalled,
    Failed(String),
}

impl State {
    fn is_failure(&self) -> bool {
        matches!(self, State::Errors(_) | State::Failed(_))
    }

    fn word(&self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Errors(_) => "errors",
            State::NotInstalled => "not installed",
            State::Failed(_) => "failed to run",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            State::Errors(count) => Some(format!("{count} problem(s)")),
            State::Failed(reason) => Some(reason.clone()),
            State::Ok | State::NotInstalled => None,
        }
    }

    fn describe(&self) -> String {
        match self.detail() {
            Some(detail) => format!("{}: {detail}", self.word()),
            None => self.word().to_string(),
        }
    }
}

/// One binary's line of the footer.
struct Status {
    name: &'static str,
    scope: Scope,
    state: State,
}

/// Every report merged by path.
struct Merged<'a> {
    file: Option<&'a str>,
    shadowed: &'a [String],
    rows: Vec<Row<'a>>,
    errors: Vec<GeneralError<'a>>,
    statuses: Vec<Status>,
}

impl<'a> Merged<'a> {
    fn new(checks: &'a [BinaryCheck]) -> Self {
        let mut rows: Vec<Row<'a>> = Vec::new();
        let mut errors: Vec<GeneralError<'a>> = Vec::new();
        let mut statuses = Vec::new();
        let mut file = None;
        let mut shadowed: &[String] = &[];

        for (index, check) in checks.iter().enumerate() {
            let report = match &check.outcome {
                CheckOutcome::Report(report) => report,
                CheckOutcome::NotInstalled => {
                    statuses.push(status(check, State::NotInstalled));
                    continue;
                }
                CheckOutcome::Failed(reason) => {
                    statuses.push(status(check, State::Failed(reason.clone())));
                    continue;
                }
            };
            // `narsor-cli`'s own report names the file every other binary
            // was handed with `--config`.
            if index == 0 {
                file = report.file.as_deref();
                shadowed = &report.shadowed;
            }
            statuses.push(status(
                check,
                if report.is_ok() {
                    State::Ok
                } else {
                    State::Errors(report.errors.len())
                },
            ));

            for (path, setting) in &report.settings {
                match rows
                    .iter_mut()
                    .find(|row| row.path == path && row.setting == setting)
                {
                    Some(row) => row.binaries.push(check.name),
                    None => rows.push(Row {
                        path,
                        setting,
                        binaries: vec![check.name],
                    }),
                }
            }
            for error in &report.errors {
                let marks_a_row = error.path.as_ref().is_some_and(|path| {
                    report
                        .settings
                        .get(path)
                        .is_some_and(|setting| setting.error.as_ref() == Some(&error.message))
                });
                if marks_a_row {
                    continue;
                }
                match errors.iter_mut().find(|known| {
                    known.path == error.path.as_deref() && known.message == error.message
                }) {
                    Some(known) => known.binaries.push(check.name),
                    None => errors.push(GeneralError {
                        path: error.path.as_deref(),
                        message: &error.message,
                        binaries: vec![check.name],
                    }),
                }
            }
        }

        // Registry order, so a section reads the way `init` writes it; a
        // path this build does not know goes after the known ones.
        rows.sort_by_key(|row| {
            let position = SettingKey::from_key(row.path)
                .and_then(|setting| SettingKey::ALL.iter().position(|known| *known == setting))
                .unwrap_or(usize::MAX);
            (position, row.path)
        });

        Self {
            file,
            shadowed,
            rows,
            errors,
            statuses,
        }
    }

    /// The section names in print order: every known section, then any
    /// other a report named.
    fn sections(&self) -> Vec<String> {
        let mut sections: Vec<String> = Section::ALL
            .iter()
            .map(|section| section.name().to_string())
            .collect();
        for row in &self.rows {
            if !sections.iter().any(|known| known == row.section()) {
                sections.push(row.section().to_string());
            }
        }
        sections
    }

    /// The binaries that read `section` whole but sent no report, with why
    /// (R24): the section's rows, if any, come from other binaries only.
    fn missing_readers(&self, section: &str) -> Vec<String> {
        self.statuses
            .iter()
            .filter(|status| matches!(status.state, State::NotInstalled | State::Failed(_)))
            .filter(|status| {
                Section::from_name(section)
                    .is_some_and(|section| status.scope.includes_section(section))
            })
            .map(|status| format!("{}: {}", status.name, status.state.describe()))
            .collect()
    }

    fn table(&self) -> String {
        let mut text = String::new();
        let cells: Vec<(String, String)> = self
            .rows
            .iter()
            .map(|row| match (row.value(), &row.setting.source) {
                (Some(value), Some(source)) => (value, render_source(source)),
                _ => ("(error)".to_string(), "-".to_string()),
            })
            .collect();
        let setting_width = self
            .rows
            .iter()
            .map(|row| row.path.len())
            .chain(["SETTING".len()])
            .max()
            .unwrap_or_default();
        let value_width = cells
            .iter()
            .map(|(value, _)| value.chars().count())
            .chain(["VALUE".len()])
            .max()
            .unwrap_or_default();
        let source_width = cells
            .iter()
            .map(|(_, source)| source.chars().count())
            .chain(["SOURCE".len()])
            .max()
            .unwrap_or_default();
        let line = |text: &mut String, setting: &str, value: &str, source: &str, rest: &str| {
            let row = format!(
                "{setting:<setting_width$}  {value:<value_width$}  {source:<source_width$}  {rest}"
            );
            text.push_str(row.trim_end());
            text.push('\n');
        };

        line(&mut text, "SETTING", "VALUE", "SOURCE", "READ BY");
        for section in self.sections() {
            let rows: Vec<usize> = (0..self.rows.len())
                .filter(|index| self.rows[*index].section() == section)
                .collect();
            let missing = self.missing_readers(&section);
            match (rows.is_empty(), missing.is_empty()) {
                (true, true) => continue,
                (_, true) => text.push_str(&format!("[{section}]\n")),
                (_, false) => text.push_str(&format!("[{section}] ({})\n", missing.join("; "))),
            }
            for index in rows {
                let row = &self.rows[index];
                let (value, source) = &cells[index];
                line(&mut text, row.path, value, source, &row.binaries.join(", "));
                if let Some(error) = &row.setting.error {
                    line(&mut text, "", &format!("error: {error}"), "", "");
                }
            }
        }

        if let Some(file) = self.file {
            text.push_str(&format!("\nfile: {file}\n"));
        }
        for shadowed in self.shadowed {
            text.push_str(&format!("shadowed: {shadowed}\n"));
        }
        // Everything wrong that marks no row: an unknown key, a syntax error,
        // a candidate that is not a readable file, an overruled value.
        if !self.errors.is_empty() {
            text.push_str("\nerrors:\n");
            for error in &self.errors {
                let path = error
                    .path
                    .map(|path| format!("{path}: "))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "  {path}{} ({})\n",
                    error.message,
                    error.binaries.join(", ")
                ));
            }
        }

        let name_width = self
            .statuses
            .iter()
            .map(|status| status.name.len())
            .chain(["BINARY".len()])
            .max()
            .unwrap_or_default();
        text.push_str(&format!("\n{:<name_width$}  STATUS\n", "BINARY"));
        for status in &self.statuses {
            text.push_str(&format!(
                "{:<name_width$}  {}\n",
                status.name,
                status.state.describe()
            ));
        }
        text
    }

    /// The machine-readable shape, alone on stdout.
    ///
    /// The version number is the point of the envelope: this is the contract
    /// a `jq` invocation in someone's runbook will be written against, and a
    /// later change of shape should be a recognisable change rather than a
    /// breakage (D-O). Version 2 lists the settings as rows, because one
    /// path can carry different values for different binaries.
    fn json(&self) -> String {
        let settings: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "path": row.path,
                    "value": row.value(),
                    "source": row.setting.source,
                    "error": row.setting.error,
                    "sensitive": row.setting.sensitive,
                    "binaries": row.binaries,
                })
            })
            .collect();
        let document = serde_json::json!({
            "version": crate::config::check::CHECK_REPORT_VERSION,
            "file": self.file,
            "shadowed": self.shadowed,
            "settings": settings,
            "errors": self.errors.iter().map(|error| serde_json::json!({
                "path": error.path,
                "message": error.message,
                "binaries": error.binaries,
            })).collect::<Vec<_>>(),
            "binaries": self.statuses.iter().map(|status| serde_json::json!({
                "name": status.name,
                "status": status.state.word(),
                "detail": status.state.detail(),
            })).collect::<Vec<_>>(),
        });
        let mut text = serde_json::to_string_pretty(&document).expect("the document is plain data");
        text.push('\n');
        text
    }
}

fn status(check: &BinaryCheck, state: State) -> Status {
    Status {
        name: check.name,
        scope: check.scope,
        state,
    }
}

/// `env (NARSOR_GLOBAL__ROOT)`, `flag (--root)`, `built-in`; a file is
/// only `file`, because the footer names the one file every binary read
/// (the JSON keeps the path).
fn render_source(source: &CheckedSource) -> String {
    match &source.detail {
        Some(detail) if source.kind != "file" => format!("{} ({detail})", source.kind),
        _ => source.kind.clone(),
    }
}
