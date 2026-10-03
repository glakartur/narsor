//! `narsor config show` (U6).
//!
//! Driven through the real binary, because what is being asserted *is* the
//! output and the exit code — the two things this command publishes.
//!
//! `HOME` and `XDG_CONFIG_HOME` point inside each test's own temporary root,
//! so the cascade can never reach the developer's own `~/.config`.

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

/// The exit code of a configuration that does not hold up.
const EXIT_FAILURE: i32 = 1;

/// The exit code of a wrong invocation.
const EXIT_USAGE: i32 = 2;

/// Every setting the command must account for (R7).
const EVERY_SETTING: [&str; 17] = [
    "catalog_dir",
    "state_dir",
    "work_dir",
    "agent_harness",
    "agent_timeout",
    "log_level",
    "tui_refresh_interval",
    "default_backend",
    "default_profile",
    "server_mode",
    "server_bind",
    "oidc_issuer",
    "oidc_audience",
    "owner_subject",
    "tls_termination",
    "trusted_proxies",
    "cors_allowed_origins",
];

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-show-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the user's configuration file.
    fn config(&self, contents: &str) -> PathBuf {
        let dir = self.path.join("config/narsor");
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

/// Runs the binary in the temporary root, with a controlled environment.
fn show(root: &TempRoot, environment: &[(&str, &str)], args: &[&str]) -> Invocation {
    let mut command = Command::new(env!("CARGO_BIN_EXE_narsor"));
    command
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .arg("config")
        .arg("show");
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

/// The JSON document the command printed.
fn json(invocation: &Invocation) -> serde_json::Value {
    serde_json::from_str(&invocation.stdout)
        .unwrap_or_else(|error| panic!("stdout must be JSON ({error}): {}", invocation.stdout))
}

#[test]
fn every_setting_is_listed_even_the_ones_nobody_touched() {
    needs_no_system_config!();
    // R7: the question is "what is in force", not "what did I change".
    let root = TempRoot::new();

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    for setting in EVERY_SETTING {
        assert!(
            invocation.stdout.contains(setting),
            "{setting} is missing: {}",
            invocation.stdout
        );
    }
    assert_eq!(
        invocation.stdout.matches("built-in").count(),
        EVERY_SETTING.len(),
        "with nothing configured every source is the built-in: {}",
        invocation.stdout
    );
}

#[test]
fn a_flag_shows_as_the_flag_even_when_the_file_names_the_same_setting() {
    needs_no_system_config!();
    // AE1.
    let root = TempRoot::new();
    root.config("state_dir: /from/the/file\n");

    let invocation = show(&root, &[], &["--root", "/from/the/flag"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let row = state_dir_row(&invocation.stdout);
    assert!(row.contains("/from/the/flag"), "{row}");
    assert!(row.contains("--root"), "the source names the flag: {row}");
    assert!(!row.contains("/from/the/file"), "{row}");
}

#[test]
fn a_value_from_the_file_shows_the_path_and_one_from_the_environment_the_variable() {
    needs_no_system_config!();
    // R8: the concrete thing, never the category.
    let root = TempRoot::new();
    let config = root.config("state_dir: /from/the/file\n");

    let invocation = show(&root, &[], &[]);
    let row = state_dir_row(&invocation.stdout);
    assert!(
        row.contains(&config.display().to_string()),
        "the path of the file that was actually read: {row}"
    );

    let invocation = show(&root, &[("NARSOR_AGENT_HARNESS", "opencode")], &[]);
    let row = row_for(&invocation.stdout, "agent_harness");
    assert!(row.contains("opencode"), "{row}");
    assert!(
        row.contains("NARSOR_AGENT_HARNESS"),
        "the source names the variable: {row}"
    );
}

#[test]
fn a_shadowed_candidate_is_named_beside_the_winner() {
    needs_no_system_config!();
    // AE5: the operator has to be able to see that the file feeding this run
    // is not the one they edited.
    let root = TempRoot::new();
    let winner = root.config("log_level: info\n");
    let loser = root.path().join("config/narsor/config.yml");
    fs::write(&loser, "log_level: debug\n").expect("write the loser");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        invocation
            .stdout
            .contains(&format!("file: {}", winner.display())),
        "{}",
        invocation.stdout
    );
    assert!(
        invocation
            .stdout
            .contains(&format!("shadowed: {}", loser.display())),
        "the loser is named, not merged and not forgotten: {}",
        invocation.stdout
    );
}

#[test]
fn the_json_form_carries_a_structured_source_and_a_version() {
    needs_no_system_config!();
    // R10 and D-O: nothing downstream should have to parse "file (/etc/…)",
    // and a later change of shape should be recognisable.
    let root = TempRoot::new();
    let config = root.config("state_dir: /from/the/file\n");

    let invocation = show(&root, &[], &["--format", "json"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let document = json(&invocation);
    assert_eq!(document["version"], 1);
    assert_eq!(document["file"], config.display().to_string());

    let source = &document["settings"]["state_dir"]["source"];
    assert_eq!(source["kind"], "file");
    assert_eq!(source["detail"], config.display().to_string());
    assert_eq!(document["settings"]["state_dir"]["value"], "/from/the/file");

    for setting in EVERY_SETTING {
        assert!(
            document["settings"].get(setting).is_some(),
            "{setting} is missing from the JSON"
        );
    }
}

#[test]
fn it_works_without_a_home_and_without_the_state_directory_existing() {
    needs_no_system_config!();
    // R9: no writable home, no writable filesystem, nothing created.
    let root = TempRoot::new();
    let absent = root.path().join("not-there");

    let mut command = Command::new(env!("CARGO_BIN_EXE_narsor"));
    let output = command
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .args(["--root", &absent.display().to_string(), "config", "show"])
        .output()
        .expect("run the narsor binary");

    assert_eq!(
        output.status.code(),
        Some(EXIT_SUCCESS),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !absent.exists(),
        "showing the configuration must not create the data directory"
    );
}

#[test]
fn an_empty_file_is_reported_as_the_winner_with_every_value_built_in() {
    needs_no_system_config!();
    // D-H: it looks contradictory and it is correct — the file is in force and
    // it sets nothing.
    let root = TempRoot::new();
    let config = root.config("");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(invocation
        .stdout
        .contains(&format!("file: {}", config.display())));
    assert_eq!(
        invocation.stdout.matches("built-in").count(),
        EVERY_SETTING.len(),
        "{}",
        invocation.stdout
    );
}

#[test]
fn a_broken_key_is_marked_while_the_rest_of_the_table_still_prints() {
    needs_no_system_config!();
    // D-D: the one command that survives what it exists to diagnose.
    let root = TempRoot::new();
    let config = root.config("state_dir: /from/the/file\nagent_timeout: tomorrow\n");

    let invocation = show(&root, &[], &[]);

    assert_eq!(
        invocation.code, EXIT_FAILURE,
        "it still fails, so a script is not misled"
    );
    assert!(
        invocation.stdout.contains("(error)"),
        "the broken row is marked: {}",
        invocation.stdout
    );
    assert!(
        invocation.stdout.contains("tomorrow"),
        "and the complaint is printed: {}",
        invocation.stdout
    );
    assert!(
        state_dir_row(&invocation.stdout).contains("/from/the/file"),
        "the key the operator got right is still shown as in force: {}",
        invocation.stdout
    );
    assert!(invocation.stdout.contains(&config.display().to_string()));
}

#[test]
fn an_unknown_key_is_reported_while_list_in_the_same_directory_refuses_outright() {
    needs_no_system_config!();
    // AE3, and the contrast that makes D-D worth having.
    let root = TempRoot::new();
    root.config("lgo_level: info\n");

    let invocation = show(&root, &[], &[]);
    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        invocation.stdout.contains("state_dir"),
        "the table is printed: {}",
        invocation.stdout
    );
    assert!(
        invocation.stdout.contains("lgo_level"),
        "{}",
        invocation.stdout
    );

    let listing = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .arg("list")
        .output()
        .expect("run the narsor binary");

    assert_eq!(listing.status.code(), Some(EXIT_FAILURE));
    assert!(
        String::from_utf8_lossy(&listing.stdout).is_empty(),
        "`list` prints no table at all"
    );
}

#[test]
fn two_broken_keys_are_both_marked() {
    needs_no_system_config!();
    let root = TempRoot::new();
    root.config("agent_timeout: tomorrow\nlog_level: shouting\n");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert_eq!(
        invocation.stdout.matches("(error)").count(),
        2,
        "both rows, not just the first: {}",
        invocation.stdout
    );
}

#[test]
fn the_json_form_stays_valid_json_when_the_file_is_broken() {
    needs_no_system_config!();
    // D-D applies to both formats identically.
    let root = TempRoot::new();
    root.config("lgo_level: info\nagent_timeout: tomorrow\n");

    let invocation = show(&root, &[], &["--format", "json"]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    let document = json(&invocation);
    assert_eq!(document["version"], 1);
    assert!(
        document["settings"]["agent_timeout"]["error"].is_string(),
        "the broken key carries its complaint: {}",
        invocation.stdout
    );
    assert!(document["settings"]["agent_timeout"]["value"].is_null());
    assert_eq!(
        document["errors"].as_array().expect("a list").len(),
        2,
        "both faults are carried structurally"
    );
}

#[test]
fn a_directory_under_the_candidate_path_still_prints_a_table() {
    needs_no_system_config!();
    // D-G through the command: the failure a ConfigMap produces is exactly the
    // one an operator reaches for this tool to understand.
    let root = TempRoot::new();
    fs::create_dir_all(root.path().join("config/narsor/config.yaml"))
        .expect("mount a directory over the file path");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    for setting in EVERY_SETTING {
        assert!(
            invocation.stdout.contains(setting),
            "the full table of built-in values is still printed: {}",
            invocation.stdout
        );
    }
    assert!(
        invocation.stdout.contains("config.yaml") && invocation.stdout.contains("directory"),
        "the path and the reason are named: {}",
        invocation.stdout
    );
}

#[test]
fn a_config_path_that_is_not_there_is_a_usage_error_with_no_table() {
    needs_no_system_config!();
    // The exception to the exception: the operator named a file that does not
    // exist, and there is nothing to diagnose.
    let root = TempRoot::new();
    let missing = root.path().join("nope.yaml");

    let invocation = show(&root, &[], &["--config", &missing.display().to_string()]);

    assert_eq!(invocation.code, EXIT_USAGE, "{}", invocation.stderr);
    assert!(
        invocation.stdout.is_empty(),
        "no table at all: {}",
        invocation.stdout
    );
    assert!(invocation.stderr.contains("nope.yaml"));
}

#[test]
fn the_json_on_stdout_is_not_polluted_by_logging() {
    needs_no_system_config!();
    // The two-stream rule, under the condition that breaks it most easily.
    let root = TempRoot::new();
    root.config("log_level: info\n");

    let invocation = show(&root, &[("RUST_LOG", "debug")], &["--format", "json"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let document = json(&invocation);
    assert_eq!(document["settings"]["log_level"]["source"]["kind"], "env");
    assert_eq!(
        document["settings"]["log_level"]["source"]["detail"], "RUST_LOG",
        "the variable that actually took effect is the one named (D-K)"
    );
}

/// The table row for one setting.
fn row_for<'a>(output: &'a str, setting: &str) -> &'a str {
    output
        .lines()
        .find(|line| line.starts_with(setting))
        .unwrap_or_else(|| panic!("no row for {setting} in:\n{output}"))
}

/// The table row for the data directory.
fn state_dir_row(output: &str) -> &str {
    row_for(output, "state_dir")
}

#[test]
fn a_bad_value_in_the_environment_marks_its_row_too() {
    needs_no_system_config!();
    // The same treatment a bad value in the file gets: an operator should not
    // have to know which layer produced a fault to find the row it belongs to.
    let root = TempRoot::new();

    let invocation = show(&root, &[("NARSOR_AGENT_TIMEOUT", "tomorrow")], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    let row = row_for(&invocation.stdout, "agent_timeout");
    assert!(row.contains("(error)"), "{row}");
    assert!(
        invocation.stdout.contains("NARSOR_AGENT_TIMEOUT"),
        "and the variable is named: {}",
        invocation.stdout
    );
}

#[test]
fn a_long_path_is_printed_whole_rather_than_clipped_to_the_column() {
    needs_no_system_config!();
    // The one question this command answers is "what is in force"; a path cut
    // at the column width is a wrong answer to it (R8).
    let root = TempRoot::new();
    let long = "/srv/a/deliberately/long/path/to/the/narsor/data/directory";

    let invocation = show(&root, &[], &["--root", long]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        invocation.stdout.contains(long),
        "the whole path must be there: {}",
        invocation.stdout
    );
    assert!(
        !invocation.stdout.contains('…'),
        "and nothing may be elided: {}",
        invocation.stdout
    );
}

#[test]
fn a_fault_in_a_layer_that_lost_does_not_mask_the_value_in_force() {
    needs_no_system_config!();
    // R2 is per setting and per layer: a bad harness in the file is still a
    // fault worth reporting, but the environment outranked it, so the row has
    // to show what will actually run.
    let root = TempRoot::new();
    root.config("agent_harness: bogus\n");

    let invocation = show(&root, &[("NARSOR_AGENT_HARNESS", "opencode")], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE, "the fault is still reported");
    let row = row_for(&invocation.stdout, "agent_harness");
    assert!(
        row.contains("opencode") && row.contains("NARSOR_AGENT_HARNESS"),
        "the row shows the value in force and where it came from: {row}"
    );
    assert!(
        !row.contains("(error)"),
        "nothing was lost to the fault, so nothing is masked: {row}"
    );
    assert!(
        invocation.stdout.contains("bogus"),
        "and the complaint is still printed: {}",
        invocation.stdout
    );
}

#[test]
fn a_fault_that_cost_the_value_does_mask_the_row() {
    needs_no_system_config!();
    // The other half: with nothing to outrank it, the fault is why the
    // built-in is in force, and the row says so.
    let root = TempRoot::new();
    root.config("agent_harness: bogus\n");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    let row = row_for(&invocation.stdout, "agent_harness");
    assert!(row.contains("(error)"), "{row}");
}

#[test]
fn the_closing_message_does_not_promise_a_marker_that_is_not_there() {
    needs_no_system_config!();
    // A cascade failure marks no row at all; sending the operator hunting for
    // one is worse than saying nothing.
    let root = TempRoot::new();
    fs::create_dir_all(root.path().join("config/narsor/config.yaml"))
        .expect("mount a directory over the file path");

    let invocation = show(&root, &[], &[]);

    assert_eq!(invocation.code, EXIT_FAILURE);
    assert!(
        !invocation.stdout.contains("(error)"),
        "no row is marked here: {}",
        invocation.stdout
    );
    assert!(
        !invocation.stderr.contains("marked"),
        "so the message must not point at a marker: {}",
        invocation.stderr
    );
}

#[test]
fn the_tui_refresh_interval_is_shown_from_the_file_and_from_the_environment() {
    needs_no_system_config!();
    // KTD16: the interval is an ordinary setting, visible like every other.
    let root = TempRoot::new();

    let invocation = show(&root, &[], &[]);
    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let row = row_for(&invocation.stdout, "tui_refresh_interval");
    assert!(row.contains("10s") && row.contains("built-in"), "{row}");

    let config = root.config("tui_refresh_interval: 30\n");
    let invocation = show(&root, &[], &[]);
    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let row = row_for(&invocation.stdout, "tui_refresh_interval");
    assert!(row.contains("30s"), "{row}");
    assert!(row.contains(&config.display().to_string()), "{row}");

    let invocation = show(&root, &[("NARSOR_TUI_REFRESH_INTERVAL", "5s")], &[]);
    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    let row = row_for(&invocation.stdout, "tui_refresh_interval");
    assert!(row.contains("5s"), "{row}");
    assert!(row.contains("NARSOR_TUI_REFRESH_INTERVAL"), "{row}");
}
