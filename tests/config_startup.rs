//! The order of the first three steps, through the real binary (U5).
//!
//! These are the things a unit test cannot prove: that a broken file actually
//! stops a subcommand, that the data directory a command works in is the
//! resolved one rather than the raw flag, and that the silence of an
//! unconfigured run survived the change of startup order (D-N).
//!
//! Every test gets its own temporary root, with `HOME` and `XDG_CONFIG_HOME`
//! redirected into it, so the cascade can never reach the developer's own
//! `~/.config`. `/etc/narsor/` is not writable by a test and is therefore
//! never a factor.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let mut command = Command::new(env!("CARGO_BIN_EXE_narsor"));
    command
        .current_dir(root.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"));
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

/// The fixture workflow whose provisioning emits log records.
fn clone_workflow() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli/clone.yaml")
        .display()
        .to_string()
}

/// The announcing script the fixtures run.
fn announce_script() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli/announce.sh")
        .display()
        .to_string()
}

/// A git repository with one empty commit, for the workspace to clone.
fn source_repository(root: &TempRoot) -> String {
    let path = root.path().join("src");
    fs::create_dir_all(&path).expect("create the source repository");
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&path)
            .args(args)
            .output()
            .expect("run git");
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@example.com",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "init",
    ]);
    path.display().to_string()
}

#[test]
fn a_broken_file_stops_every_subcommand_with_the_file_named() {
    // AE3.
    let root = TempRoot::new();
    let config = root.config("lgo_level: info\n");

    for arguments in [
        vec!["list"],
        vec!["validate", "compound-engineering"],
        vec!["run", "compound-engineering"],
        vec!["tui"],
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

#[test]
fn the_config_flag_works_after_the_subcommand_too() {
    let root = TempRoot::new();
    let elsewhere = root.path().join("mine.yaml");
    fs::write(&elsewhere, "log_level: warn\n").expect("write the file");

    let invocation = cli(
        &root,
        &[],
        &["list", "--config", &elsewhere.display().to_string()],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
}

#[test]
fn a_good_file_does_not_disturb_anything() {
    let root = TempRoot::new();
    root.config("log_level: off\nagent_harness: opencode\n");

    let invocation = cli(&root, &[], &["list"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(invocation.stdout.contains("No executions"));
}

#[test]
fn the_data_directory_from_the_file_is_the_one_the_command_works_in() {
    let root = TempRoot::new();
    let store = root.path().join("from-the-file");
    root.config(&format!("data_dir: {}\n", store.display()));

    let invocation = cli(
        &root,
        &[],
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", source_repository(&root)),
            "--input",
            &format!("script_path={}", announce_script()),
        ],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        store.join("executions").exists(),
        "the execution must be stored under the configured directory, not ./.narsor"
    );
    assert!(
        !root.path().join(".narsor").exists(),
        "and nothing may be written to the built-in one"
    );
}

#[test]
fn the_flag_still_beats_the_file_for_the_data_directory() {
    // AE1, through the binary.
    let root = TempRoot::new();
    root.config(&format!(
        "data_dir: {}\n",
        root.path().join("from-the-file").display()
    ));
    let from_the_flag = root.path().join("from-the-flag");

    let invocation = cli(
        &root,
        &[],
        &[
            "--root",
            &from_the_flag.display().to_string(),
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", source_repository(&root)),
            "--input",
            &format!("script_path={}", announce_script()),
        ],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(from_the_flag.join("executions").exists());
    assert!(!root.path().join("from-the-file").exists());
}

#[test]
fn nothing_configured_means_not_one_line_of_log() {
    // D-N: exactly today's behaviour, and the startup order did not change it.
    let root = TempRoot::new();

    let invocation = cli(
        &root,
        &[],
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", source_repository(&root)),
            "--input",
            &format!("script_path={}", announce_script()),
        ],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        !invocation.stderr.contains("DEBUG") && !invocation.stderr.contains("INFO"),
        "no subscriber is installed, so no record is written: {}",
        invocation.stderr
    );
}

#[test]
fn a_level_in_the_file_changes_what_reaches_stderr() {
    let root = TempRoot::new();
    root.config("log_level: debug\n");

    let invocation = cli(
        &root,
        &[],
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", source_repository(&root)),
            "--input",
            &format!("script_path={}", announce_script()),
        ],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        invocation.stderr.contains("DEBUG"),
        "the level from the file has to reach the subscriber: {}",
        invocation.stderr
    );
    assert!(
        invocation.stdout.contains("Created execution"),
        "and the logs must not have taken over stdout"
    );
}

#[test]
fn rust_log_still_overrides_the_configured_level() {
    // D-K, through the binary: the file says be quiet, RUST_LOG says talk.
    let root = TempRoot::new();
    root.config("log_level: off\n");

    let invocation = cli(
        &root,
        &[("RUST_LOG", "debug")],
        &[
            "run",
            &clone_workflow(),
            "--input",
            &format!("repository={}", source_repository(&root)),
            "--input",
            &format!("script_path={}", announce_script()),
        ],
    );

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(invocation.stderr.contains("DEBUG"), "{}", invocation.stderr);
}

#[test]
fn the_tui_log_file_lands_in_the_resolved_data_directory() {
    // The interface cannot be driven from a test, but the file it would write
    // into is opened before the terminal is touched — so the failure to start
    // still leaves the log behind, in the directory the configuration named.
    let root = TempRoot::new();
    let store = root.path().join("from-the-file");
    root.config(&format!(
        "data_dir: {}\nlog_level: debug\n",
        store.display()
    ));

    let invocation = cli(&root, &[], &["tui"]);

    assert_ne!(
        invocation.code, EXIT_USAGE,
        "the invocation itself is fine: {}",
        invocation.stderr
    );
    assert!(
        store.join("narsor.log").exists(),
        "the log belongs under the configured data directory, not under the raw flag"
    );
}

#[test]
fn an_unconfigured_tui_creates_neither_the_directory_nor_the_log() {
    // D-N: today a `tui` run with nothing configured leaves no trace, and it
    // still must not.
    let root = TempRoot::new();

    let invocation = cli(&root, &[], &["tui"]);

    assert_ne!(invocation.code, EXIT_USAGE, "{}", invocation.stderr);
    assert!(
        !root.path().join(".narsor").exists(),
        "no data directory may be created just to run the interface"
    );
}
