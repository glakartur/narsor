//! The order of the first three steps, through the real binary (U5).
//!
//! These are the things a unit test cannot prove: that a broken file actually
//! stops a subcommand, that the roots a command works in are the
//! resolved one rather than the raw flag, and that the silence of an
//! unconfigured run survived the change of startup order (D-N).
//!
//! Every test gets its own temporary root, with `HOME` and `XDG_CONFIG_HOME`
//! redirected into it, so the cascade can never reach the developer's own
//! `~/.config`. `/etc/narsor/` is not writable by a test and is therefore
//! never a factor.

mod engine_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether this machine has a real system-wide configuration file.
///
/// The third step of the cascade is `/etc/narsor/`, which a test cannot
/// redirect through the environment the way it redirects `HOME` and
/// `XDG_CONFIG_HOME`. A machine that has one — and this feature exists to
/// encourage exactly that — would feed it into every assertion below. Skipping
/// with a word is better than failing with a mystery.
fn system_config_present() -> bool {
    ["yaml", "yml"].iter().any(|extension| {
        Path::new("/etc/narsor")
            .join(format!("config.{extension}"))
            .exists()
    })
}

/// Leaves the test early, saying why, when the machine has one.
macro_rules! needs_no_system_config {
    () => {
        if system_config_present() {
            eprintln!("skipped: this machine has /etc/narsor/config.yaml, which the cascade finds");
            return;
        }
    };
}

/// The exit code of a successful command.
const EXIT_SUCCESS: i32 = 0;

/// The exit code of a failure — including a configuration that does not hold
/// up.
const EXIT_FAILURE: i32 = 1;

/// The exit code of a wrong invocation.
const EXIT_USAGE: i32 = 2;

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-startup-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// `$XDG_CONFIG_HOME/narsor`, the directory the cascade searches.
    fn config_dir(&self) -> PathBuf {
        self.path.join("config/narsor")
    }

    /// Writes the user's configuration file.
    fn config(&self, contents: &str) -> PathBuf {
        let dir = self.config_dir();
        fs::create_dir_all(&dir).expect("create the config directory");
        let path = dir.join("config.yaml");
        fs::write(&path, contents).expect("write the config");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// What one invocation produced.
struct Invocation {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the binary with `HOME` and `XDG_CONFIG_HOME` inside the temporary
/// root, and without `--root` unless the caller passes one.
fn cli(root: &TempRoot, environment: &[(&str, &str)], args: &[&str]) -> Invocation {
    let mut command = Command::new(env!("CARGO_BIN_EXE_narsor-cli"));
    command
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("NARSOR_LOCAL_WORKSPACE_ROOTS", root.path());
    for (name, value) in environment {
        command.env(name, value);
    }

    let output = command.args(args).output().expect("run the narsor binary");
    Invocation {
        code: output.status.code().expect("the process exited normally"),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn a_broken_file_stops_every_subcommand_with_the_file_named() {
    needs_no_system_config!();
    // AE3.
    let root = TempRoot::new();
    let config = root.config("lgo_level: info\n");

    for arguments in [
        vec!["list"],
        vec!["validate", "template"],
        vec!["run", "template"],
    ] {
        let invocation = cli(&root, &[], &arguments);

        assert_eq!(
            invocation.code,
            EXIT_FAILURE,
            "`{}` must refuse to start: {}{}",
            arguments.join(" "),
            invocation.stdout,
            invocation.stderr
        );
        assert!(
            invocation.stderr.contains(&config.display().to_string()),
            "the message must name the file: {}",
            invocation.stderr
        );
        assert!(
            invocation.stdout.is_empty(),
            "nothing ran, so nothing is on stdout: {}",
            invocation.stdout
        );
    }
}

#[test]
fn a_broken_file_reports_every_fault_at_once() {
    needs_no_system_config!();
    let root = TempRoot::new();
    root.config("nonsense: 1\nlog_level: shouting\n");

    let invocation = cli(&root, &[], &["list"]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        invocation.stderr.contains("nonsense"),
        "{}",
        invocation.stderr
    );
    assert!(
        invocation.stderr.contains("shouting"),
        "{}",
        invocation.stderr
    );
}

#[test]
fn a_config_path_that_is_not_there_is_a_usage_error() {
    needs_no_system_config!();
    let root = TempRoot::new();
    let missing = root.path().join("nope.yaml");

    let invocation = cli(
        &root,
        &[],
        &["--config", &missing.display().to_string(), "list"],
    );

    assert_eq!(
        invocation.code, EXIT_USAGE,
        "the operator named a file that is not there: {}",
        invocation.stderr
    );
    assert!(invocation.stderr.contains("nope.yaml"));
}

// ---------------------------------------------------------------------------
// The setting that used to exist
// ---------------------------------------------------------------------------

/// AE4 — `data_dir` in the file stops the command and names its successors.
#[test]
fn data_dir_in_the_file_stops_the_command_and_names_what_replaced_it() {
    needs_no_system_config!();
    let root = TempRoot::new();
    root.config("data_dir: /srv/narsor\n");

    let invocation = cli(&root, &[], &["list"]);

    assert_eq!(invocation.code, EXIT_FAILURE, "{}", invocation.stdout);
    for successor in ["catalog_dir", "state_dir", "work_dir"] {
        assert!(
            invocation.stderr.contains(successor),
            "the message must name {successor}: {}",
            invocation.stderr
        );
    }
    assert!(
        !invocation.stderr.contains("known keys are"),
        "a withdrawn setting is not a typo, and must not be answered like one: {}",
        invocation.stderr
    );
}

/// AE4 — and the same for the variable, which used to be ignored in silence.
///
/// This is the half that could not work before: resolution asks for one
/// variable per *known* setting, so a withdrawn one was never looked up. A run
/// carrying `NARSOR_DATA_DIR` would have quietly used `./.narsor` instead —
/// writing an operator's executions somewhere they never asked for.
#[test]
fn the_data_dir_variable_cannot_be_ignored_in_silence() {
    needs_no_system_config!();
    let root = TempRoot::new();

    let invocation = cli(&root, &[("NARSOR_DATA_DIR", "/srv/narsor")], &["list"]);

    assert_eq!(invocation.code, EXIT_FAILURE, "{}", invocation.stdout);
    assert!(
        invocation.stderr.contains("NARSOR_DATA_DIR"),
        "the message must name the variable: {}",
        invocation.stderr
    );
    for successor in ["catalog_dir", "state_dir", "work_dir"] {
        assert!(
            invocation.stderr.contains(successor),
            "{}",
            invocation.stderr
        );
    }
}

/// D-J — it stops every subcommand, but not the one that diagnoses them.
#[test]
fn config_show_survives_a_withdrawn_setting_and_still_prints_the_table() {
    needs_no_system_config!();
    let root = TempRoot::new();
    root.config("data_dir: /srv/narsor\n");

    let invocation = cli(&root, &[], &["config", "show"]);

    assert_eq!(
        invocation.code, EXIT_FAILURE,
        "it still fails, so a script branching on the code is not misled"
    );
    for setting in ["catalog_dir", "state_dir", "work_dir", "log_level"] {
        assert!(
            invocation.stdout.contains(setting),
            "the table must still print {setting}: {}",
            invocation.stdout
        );
    }
    assert!(
        !invocation.stdout.contains("data_dir "),
        "but the withdrawn key is not a row — it has no value and no source: {}",
        invocation.stdout
    );
}

/// AE3 — `--root` is one knob that sets all three, and says so.
#[test]
fn the_root_flag_sets_all_three_and_every_row_names_it() {
    needs_no_system_config!();
    let root = TempRoot::new();
    let target = root.path().join("everything");

    let invocation = cli(
        &root,
        &[],
        &["--root", target.to_str().expect("utf-8"), "config", "show"],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    for setting in ["catalog_dir", "state_dir", "work_dir"] {
        let row = invocation
            .stdout
            .lines()
            .find(|line| line.starts_with(setting))
            .unwrap_or_else(|| panic!("no row for {setting}:\n{}", invocation.stdout));
        assert!(
            row.contains(target.to_str().expect("utf-8")),
            "{setting} must carry the flag's value: {row}"
        );
        assert!(
            row.contains("flag (--root)"),
            "{setting} must name the flag as its source: {row}"
        );
    }
}

// ---------------------------------------------------------------------------
// Server security mode (U7)
// ---------------------------------------------------------------------------
