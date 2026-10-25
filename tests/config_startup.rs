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
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
    needs_no_system_config!();
    // AE3.
    let root = TempRoot::new();
    let config = root.config("lgo_level: info\n");

    for arguments in [
        vec!["list"],
        vec!["validate", "compound-engineering"],
        vec!["run", "compound-engineering"],
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

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn the_config_flag_works_after_the_subcommand_too() {
    needs_no_system_config!();
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
#[ignore = "starts narsor-server; run with --ignored"]
fn a_good_file_does_not_disturb_anything() {
    needs_no_system_config!();
    let root = TempRoot::new();
    root.config("log_level: off\nagent_harness: opencode\n");

    let invocation = cli(&root, &[], &["list"]);

    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(invocation.stdout.contains("No executions"));
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn the_state_directory_from_the_file_is_the_one_the_command_works_in() {
    needs_no_system_config!();
    let root = TempRoot::new();
    let store = root.path().join("from-the-file");
    // All three, because they are independent now: setting one and expecting
    // the other two to follow is exactly the assumption the split removes.
    root.config(&format!(
        "catalog_dir: {0}\nstate_dir: {0}\nwork_dir: {0}\n",
        store.display()
    ));

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
        store.join("local-controller").exists(),
        "the task must be stored under the configured directory, not ./.narsor"
    );
    assert!(
        !root.path().join(".narsor").exists(),
        "and nothing may be written to the built-in one"
    );
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn the_flag_still_beats_the_file_for_the_state_directory() {
    needs_no_system_config!();
    // AE1, through the binary.
    let root = TempRoot::new();
    root.config(&format!(
        "state_dir: {}\n",
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
    assert!(from_the_flag.join("local-controller").exists());
    assert!(!root.path().join("from-the-file").exists());
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn nothing_configured_means_not_one_line_of_log() {
    needs_no_system_config!();
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
#[ignore = "starts narsor-server; run with --ignored"]
fn a_level_in_the_file_changes_what_reaches_stderr() {
    needs_no_system_config!();
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
#[ignore = "starts narsor-server; run with --ignored"]
fn rust_log_still_overrides_the_configured_level() {
    needs_no_system_config!();
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
#[ignore = "starts narsor-server; run with --ignored"]
fn the_tui_log_file_lands_in_the_resolved_state_directory() {
    needs_no_system_config!();
    // The interface cannot be driven from a test, but the file it would write
    // into is opened before the terminal is touched — so the failure to start
    // still leaves the log behind, in the directory the configuration named.
    let root = TempRoot::new();
    let store = root.path().join("from-the-file");
    root.config(&format!(
        "state_dir: {}\nlog_level: debug\n",
        store.display()
    ));

    let output = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .output()
        .expect("run the narsor TUI");
    let invocation = Invocation {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };

    assert_ne!(
        invocation.code, EXIT_USAGE,
        "the invocation itself is fine: {}",
        invocation.stderr
    );
    assert!(
        store.join("narsor.log").exists(),
        "the log belongs under the configured state directory, not under the raw flag"
    );
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn a_partly_invalid_rust_log_keeps_the_part_that_parsed() {
    needs_no_system_config!();
    // What EnvFilter::from_default_env did before the configuration layer
    // existed. Refusing the whole directive would silence a run that asked to
    // be noisy, over a typo, with nothing said.
    let root = TempRoot::new();

    let invocation = cli(
        &root,
        &[("RUST_LOG", "debug,narsor=nonsense=broken")],
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
        "the directive that parsed still applies: {}",
        invocation.stderr
    );
}

/// AE1 — three roots pointed at three places, and each holds only its own.
///
/// The assertion that matters is about what is *absent*: a consumer handed all
/// three roots and still using one would pass every "the file is where I
/// expect" check and fail here. Listing each directory is the only way to see
/// it.
#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn three_roots_pointed_apart_each_hold_only_what_belongs_to_them() {
    needs_no_system_config!();
    let root = TempRoot::new();
    let catalog = root.path().join("catalog");
    let state = root.path().join("state");
    let work = root.path().join("work");
    std::fs::create_dir_all(&catalog).expect("create the catalogue");
    root.config(&format!(
        "catalog_dir: {}\nstate_dir: {}\nwork_dir: {}\n",
        catalog.display(),
        state.display(),
        work.display()
    ));

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

    // The catalogue is declaration only: the run puts nothing into it at all.
    assert_eq!(
        entries_of(&catalog),
        Vec::<String>::new(),
        "a run must write nothing into the catalogue — that is what lets it be \
         mounted read-only and shared between pods"
    );

    // The state root holds the local controller's task storage (executions
    // and workspaces of admitted tasks live inside it), the control plane's
    // queue (`orchestration`) and task materials, and — since U6 — `narsor-cli
    // run` transparently discovered-or-launched `narsor-server` (AE2) also left its
    // discovery file and its detached log there; nothing else.
    assert_eq!(
        entries_of(&state),
        vec![
            "daemon.json".to_string(),
            "local-controller".to_string(),
            "materials".to_string(),
            "narsor.log".to_string(),
            "orchestration".to_string(),
        ]
    );

    // The work root holds the local backend's task storage — the checkouts —
    // and nothing else.
    assert_eq!(entries_of(&work), vec!["local-controller".to_string()]);

    // And the built-in root is never touched when all three are configured.
    assert!(!root.path().join(".narsor").exists());
}

/// The names directly inside a directory, sorted; empty when it does not exist.
fn entries_of(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
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

/// Starts `narsor-server` with this process's configuration environment, but
/// never waits on it forever: a server that wrongly starts is killed after
/// `deadline`, and its exit code then reads as the signal's.
fn daemon(root: &TempRoot, environment: &[(&str, &str)], deadline: Duration) -> Invocation {
    let mut command = Command::new(engine_support::workspace_binary("narsor-server"));
    command
        .current_dir(root.path())
        .env_clear()
        .env("PATH", engine_support::fake_claude_first_on_path())
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("start the server");
    let started = Instant::now();
    while child.try_wait().expect("poll the daemon").is_none() {
        if started.elapsed() > deadline {
            // SAFETY: a plain signal to our own child.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().expect("collect the daemon");
    Invocation {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The variables of a complete network-mode configuration, against a
/// loopback issuer that is never contacted at startup.
const NETWORK_MODE: [(&str, &str); 6] = [
    ("NARSOR_SERVER_MODE", "network"),
    ("NARSOR_SERVER_BIND", "127.0.0.1:0"),
    ("NARSOR_OIDC_ISSUER", "http://127.0.0.1:9"),
    ("NARSOR_OIDC_AUDIENCE", "narsor-api"),
    ("NARSOR_TLS_TERMINATION", "proxy"),
    ("NARSOR_TRUSTED_PROXIES", "127.0.0.1"),
];

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn trusted_mode_refuses_to_listen_beyond_loopback() {
    needs_no_system_config!();
    for bind in ["0.0.0.0:7400", "[::]:7400", "[::ffff:127.0.0.1]:7400"] {
        let root = TempRoot::new();
        let invocation = daemon(
            &root,
            &[("NARSOR_SERVER_BIND", bind)],
            Duration::from_secs(20),
        );
        assert_eq!(
            invocation.code, EXIT_FAILURE,
            "{bind}: {}",
            invocation.stderr
        );
        assert!(
            invocation.stderr.contains("server_bind") && invocation.stderr.contains("loopback"),
            "{bind}: {}",
            invocation.stderr
        );
        assert!(
            !root.path().join(".narsor/daemon.json").exists(),
            "nothing was published"
        );
    }
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn network_mode_without_issuer_audience_or_tls_policy_refuses_to_start() {
    needs_no_system_config!();
    let root = TempRoot::new();
    let invocation = daemon(
        &root,
        &[("NARSOR_SERVER_MODE", "network")],
        Duration::from_secs(20),
    );
    assert_eq!(invocation.code, EXIT_FAILURE, "{}", invocation.stderr);
    for named in [
        "oidc_issuer",
        "oidc_audience",
        "tls_termination",
        "trusted_proxies",
    ] {
        assert!(
            invocation.stderr.contains(named),
            "every missing piece is named at once — {named}: {}",
            invocation.stderr
        );
    }
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn an_insecure_security_value_stops_the_daemon_with_the_key_named() {
    needs_no_system_config!();
    let cases: [(&str, &str); 4] = [
        (
            "server_mode: network\noidc_issuer: http://issuer.example\n",
            "oidc_issuer",
        ),
        ("cors_allowed_origins: [\"*\"]\n", "cors_allowed_origins"),
        ("trusted_proxies: 0.0.0.0/0\n", "trusted_proxies"),
        // Network-only settings in trusted mode are refused, not ignored.
        ("oidc_issuer: https://issuer.example\n", "server_mode"),
    ];
    for (file, named) in cases {
        let root = TempRoot::new();
        root.config(file);
        let invocation = daemon(&root, &[], Duration::from_secs(20));
        assert_eq!(
            invocation.code, EXIT_FAILURE,
            "{file}: {}",
            invocation.stderr
        );
        assert!(
            invocation.stderr.contains(named),
            "{file} must name {named}: {}",
            invocation.stderr
        );
    }
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn a_network_daemon_starts_without_the_built_in_local_controller() {
    needs_no_system_config!();
    // The built-in local controller reaches the API directly, which the
    // network mode's trusted-proxy gate refuses; it used to stop the daemon
    // with "enrollment failed" right after the claim code was printed.
    let root = TempRoot::new();
    let discovery = root.path().join(".narsor/daemon.json");
    let watcher = {
        let discovery = discovery.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(20) {
                if discovery.exists() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            false
        })
    };
    let invocation = daemon(&root, &NETWORK_MODE, Duration::from_secs(4));
    assert!(
        watcher.join().unwrap(),
        "the daemon became discoverable: {}",
        invocation.stderr
    );
    assert_eq!(invocation.code, EXIT_SUCCESS, "{}", invocation.stderr);
    assert!(
        !invocation.stderr.contains("enrollment failed"),
        "{}",
        invocation.stderr
    );
}

#[test]
#[ignore = "starts narsor-server; run with --ignored"]
fn an_unclaimed_network_daemon_prints_and_stores_a_one_time_claim_code() {
    use std::os::unix::fs::PermissionsExt;

    needs_no_system_config!();
    let root = TempRoot::new();
    let code_file = root.path().join(".narsor/auth/claim-code");
    let watcher = {
        let code_file = code_file.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(20) {
                if let Ok(metadata) = fs::metadata(&code_file) {
                    return Some((
                        metadata.permissions().mode() & 0o777,
                        fs::read_to_string(&code_file).unwrap_or_default(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        })
    };
    // Runs until the deadline, then stops on SIGTERM like any daemon.
    let invocation = daemon(&root, &NETWORK_MODE, Duration::from_secs(4));
    let (mode, code) = watcher
        .join()
        .unwrap()
        .unwrap_or_else(|| panic!("no claim code was stored: {}", invocation.stderr));
    assert_eq!(mode, 0o600, "the code is readable by the operator only");
    let code = code.trim();
    assert!(code.starts_with("nsclaim-"), "{code}");
    assert!(
        invocation.stderr.contains(code),
        "the code is printed for the operator: {}",
        invocation.stderr
    );
}
