//! Layering a value out of the flag, the environment, the file and the
//! built-in, per path and for one binary's scope.
//!
//! # Not one call to `set_var`
//!
//! The environment is built as a map and handed to
//! [`resolve_with`](narsor_engine::config::resolve::resolve_with), and the cascade is
//! given both of its directories explicitly. Nothing here reads the process
//! environment, so nothing here can race a test running beside it in the same
//! binary — which is what `std::env::set_var` would guarantee it does.

use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::config::cascade::ConfigCascade;
use narsor_engine::config::resolve::{resolve_with, Environment, Flags, Resolved};
use narsor_engine::config::{Scope, SettingKey, SettingValue, Source};

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-resolve-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn user_dir(&self) -> PathBuf {
        self.path.join("user")
    }

    fn system_dir(&self) -> PathBuf {
        self.path.join("system")
    }

    /// Writes the user's configuration file and returns its path.
    fn user_config(&self, contents: &str) -> PathBuf {
        let path = self.user_dir().join("config.yaml");
        fs::create_dir_all(self.user_dir()).expect("create the user directory");
        fs::write(&path, contents).expect("write the file");
        path
    }

    /// Resolves `scope` against this root's cascade.
    fn resolve(&self, flags: &Flags, env: &[(&str, &str)], scope: Scope) -> Resolved {
        resolve_with(flags, &environment(env), &cascade(self), scope)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An environment built from pairs, never read from the process.
fn environment(pairs: &[(&str, &str)]) -> Environment {
    pairs
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// A cascade confined to the temporary root.
fn cascade(root: &TempRoot) -> ConfigCascade {
    ConfigCascade::with_dirs(None, Some(root.user_dir()), Some(root.system_dir()))
}

/// No file anywhere.
fn nowhere() -> ConfigCascade {
    ConfigCascade::with_dirs(None, None, None)
}

fn root_flag(root: &str) -> Flags {
    Flags {
        root: Some(PathBuf::from(root)),
        config: None,
    }
}

#[test]
fn the_root_is_layered_flag_then_environment_then_file_then_built_in() {
    let root = TempRoot::new();
    let built_in = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(built_in.root(), Path::new("./.narsor"));
    assert_eq!(built_in.source(SettingKey::GlobalRoot), &Source::BuiltIn);

    let path = root.user_config("global:\n  root: /from/the/file\n");
    let file = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(file.root(), Path::new("/from/the/file"));
    assert_eq!(file.source(SettingKey::GlobalRoot), &Source::File(path));

    let env = [("NARSOR_GLOBAL__ROOT", "/from/the/environment")];
    let from_env = root.resolve(&Flags::default(), &env, Scope::SERVER);
    assert_eq!(from_env.root(), Path::new("/from/the/environment"));
    assert_eq!(
        from_env.source(SettingKey::GlobalRoot),
        &Source::Env("NARSOR_GLOBAL__ROOT".to_string()),
        "the source names the variable, not the word 'environment'"
    );

    let flag = root.resolve(&root_flag("/from/the/flag"), &env, Scope::SERVER);
    assert_eq!(flag.root(), Path::new("/from/the/flag"));
    assert_eq!(flag.source(SettingKey::GlobalRoot), &Source::Flag("--root"));
}

#[test]
fn an_environment_variable_named_after_the_path_beats_the_file() {
    let root = TempRoot::new();
    root.user_config("server:\n  bind: 127.0.0.1:7000\n");

    let resolved = root.resolve(
        &Flags::default(),
        &[("NARSOR_SERVER__BIND", "127.0.0.1:8000")],
        Scope::SERVER,
    );

    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(
        resolved.text(SettingKey::ServerBind),
        Some("127.0.0.1:8000")
    );
    assert_eq!(
        resolved.source(SettingKey::ServerBind),
        &Source::Env("NARSOR_SERVER__BIND".to_string())
    );
}

#[test]
fn server_directories_are_relative_to_the_root_unless_absolute() {
    // AE5, for the server and for every client that reads server.state.
    let root = TempRoot::new();
    for (state, expected) in [
        (Some("/var/lib/narsor-state"), "/var/lib/narsor-state"),
        (None, "/srv/narsor"),
        (Some("state"), "/srv/narsor/state"),
    ] {
        let state_line = state.map_or(String::new(), |state| format!("  state: {state}\n"));
        root.user_config(&format!(
            "global:\n  root: /srv/narsor\nserver:\n  mode: trusted\n{state_line}"
        ));
        for scope in [Scope::SERVER, Scope::WEB, Scope::TUI, Scope::CLI] {
            let resolved = root.resolve(&Flags::default(), &[], scope);
            assert!(resolved.is_ok(), "{scope:?}: {:?}", resolved.errors());
            assert_eq!(
                resolved.state_dir(),
                Path::new(expected),
                "{scope:?} {state:?}"
            );
        }
    }
}

#[test]
fn the_root_flag_alone_moves_every_server_directory() {
    let resolved = resolve_with(
        &root_flag("/x"),
        &environment(&[]),
        &nowhere(),
        Scope::SERVER,
    );

    assert_eq!(resolved.catalog_dir(), Path::new("/x"));
    assert_eq!(resolved.state_dir(), Path::new("/x"));
    assert_eq!(resolved.work_dir(), Path::new("/x"));
    assert_eq!(
        resolved.source(SettingKey::StateDir),
        &Source::BuiltIn,
        "a directory keeps the source of its own key"
    );
}

#[test]
fn with_nothing_anywhere_every_scoped_setting_is_built_in_and_nothing_fails() {
    // AE2: no file, no home, no XDG — and a working command.
    let resolved = resolve_with(&Flags::default(), &environment(&[]), &nowhere(), Scope::ALL);

    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    for setting in SettingKey::ALL {
        assert_eq!(resolved.source(setting), &Source::BuiltIn, "{setting}");
    }
    assert_eq!(
        resolved.state_dir().to_string_lossy(),
        "./.narsor",
        "the built-in directory is the root itself, not ./.narsor/."
    );
    assert_eq!(resolved.agent_harness(), "claude-code");
    assert_eq!(
        resolved.log_directive(),
        None,
        "no level, no subscriber (D-N)"
    );
}

#[test]
fn old_flat_variables_and_other_sections_variables_are_not_read() {
    let root = TempRoot::new();
    let resolved = root.resolve(
        &Flags::default(),
        &[
            ("NARSOR_SERVER_MODE", "network"),
            ("NARSOR_WEB__BIND", "bad"),
        ],
        Scope::SERVER,
    );

    assert!(
        resolved.is_ok(),
        "an unscoped variable is not checked: {:?}",
        resolved.errors()
    );
    assert_eq!(resolved.text(SettingKey::ServerMode), Some("trusted"));
}

#[test]
fn an_empty_environment_variable_counts_as_unset() {
    // D-J: that is how Kubernetes renders an unset key of a ConfigMap.
    let root = TempRoot::new();
    let path = root.user_config("server:\n  state: /from/the/file\n");

    let resolved = root.resolve(
        &Flags::default(),
        &[("NARSOR_SERVER__STATE", "")],
        Scope::SERVER,
    );

    assert_eq!(resolved.state_dir(), Path::new("/from/the/file"));
    assert_eq!(resolved.source(SettingKey::StateDir), &Source::File(path));
}

#[test]
fn rust_log_wins_over_the_level_and_is_the_source_reported() {
    // D-K: RUST_LOG takes per-target directives, so the sharper instrument
    // wins — and `config show` names the variable that actually took effect.
    let level = environment(&[("NARSOR_GLOBAL__LOG_LEVEL", "warn")]);
    let resolved = resolve_with(&Flags::default(), &level, &nowhere(), Scope::CLI);
    assert_eq!(resolved.log_directive(), Some("warn".to_string()));

    let both = environment(&[
        ("RUST_LOG", "narsor_engine::engine=debug"),
        ("NARSOR_GLOBAL__LOG_LEVEL", "warn"),
    ]);
    let resolved = resolve_with(&Flags::default(), &both, &nowhere(), Scope::CLI);
    assert_eq!(
        resolved.value(SettingKey::LogLevel),
        &SettingValue::Text("narsor_engine::engine=debug".to_string())
    );
    assert_eq!(
        resolved.source(SettingKey::LogLevel),
        &Source::Env("RUST_LOG".to_string())
    );
}

#[test]
fn a_broken_file_yields_errors_and_still_yields_every_value() {
    // D-D: resolution never returns a Result, because the decision to refuse
    // to start belongs to the caller — `config show` makes the other one.
    let root = TempRoot::new();
    let path = root.user_config("global:\n  nonsense: 1\n  log_level: shouting\n");

    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);

    assert!(!resolved.is_ok());
    assert_eq!(resolved.errors().len(), 2);
    assert!(!resolved.has_usage_error(), "this one is exit code 1");
    for error in resolved.errors() {
        assert!(error.to_string().contains(&path.display().to_string()));
    }
    assert_eq!(resolved.state_dir().to_string_lossy(), "./.narsor");
    assert_eq!(resolved.source(SettingKey::StateDir), &Source::BuiltIn);
}

#[test]
fn a_named_file_that_is_not_there_is_flagged_as_a_usage_error() {
    let root = TempRoot::new();
    let missing = root.path.join("nope.yaml");
    let cascade = ConfigCascade::with_dirs(
        Some(missing.clone()),
        Some(root.user_dir()),
        Some(root.system_dir()),
    );

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade, Scope::CLI);

    assert!(!resolved.is_ok());
    assert!(
        resolved.has_usage_error(),
        "the operator named a file that is not there: exit code 2"
    );
    assert!(resolved.errors()[0].to_string().contains("nope.yaml"));
}

#[test]
fn a_setting_given_in_the_environment_is_held_to_the_same_contract_as_the_file() {
    // R6 asks for the two to be interchangeable; they only are if a nonsense
    // value is refused in both.
    let env = environment(&[("NARSOR_TUI__REFRESH_INTERVAL", "tomorrow")]);

    let resolved = resolve_with(&Flags::default(), &env, &nowhere(), Scope::TUI);

    assert!(!resolved.is_ok());
    let message = resolved.errors()[0].to_string();
    assert!(
        message.contains("NARSOR_TUI__REFRESH_INTERVAL"),
        "{message}"
    );
    assert!(message.contains("tomorrow"), "{message}");
    assert_eq!(resolved.errors()[0].key(), Some("tui.refresh_interval"));
}

#[test]
fn resolving_writes_nothing_and_needs_no_writable_home() {
    // R9.
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::ALL);

    assert!(resolved.is_ok());
    assert!(
        !root.user_dir().exists(),
        "nothing was created to resolve a setting"
    );
}

#[test]
fn the_default_backend_and_profile_come_from_the_server_defaults_group() {
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(resolved.default_backend(), "local");
    assert_eq!(resolved.default_profile(), "default");

    root.user_config("server:\n  defaults:\n    backend: k8s-prod\n    profile: \"a b\"\n");
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    let keys: Vec<_> = resolved.errors().iter().filter_map(|e| e.key()).collect();
    assert_eq!(keys, vec!["server.defaults.profile"]);
    assert_eq!(resolved.default_backend(), "k8s-prod");
    assert_eq!(
        resolved.default_profile(),
        "default",
        "a refused value never takes effect"
    );
}

#[test]
fn local_tasks_keep_the_hosts_home_unless_the_controller_section_says_otherwise() {
    use narsor_engine::runner::{HarnessHome, HomeMode};
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::CONTROLLER);
    assert_eq!(
        resolved.local_harness_home(),
        HarnessHome {
            mode: HomeMode::Host,
            ..HarnessHome::default()
        }
    );

    root.user_config(
        "controller:\n  local:\n    home: private\n    codex_home: /home/me/.codex\n    claude_config_dir: /home/me/.claude\n    claude_profile_source: /srv/claude-profile\n",
    );
    let resolved = root.resolve(&Flags::default(), &[], Scope::CONTROLLER);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(
        resolved.local_harness_home(),
        HarnessHome {
            mode: HomeMode::Private,
            codex_home: Some(PathBuf::from("/home/me/.codex")),
            claude_config_dir: Some(PathBuf::from("/home/me/.claude")),
            claude_profile_source: Some(PathBuf::from("/srv/claude-profile")),
        }
    );

    let resolved = root.resolve(
        &Flags::default(),
        &[("NARSOR_CONTROLLER__LOCAL__CODEX_HOME", ".codex")],
        Scope::CONTROLLER,
    );
    assert!(
        resolved.errors()[0].to_string().contains("absolute"),
        "{}",
        resolved.errors()[0]
    );
}

#[test]
fn local_tasks_run_as_host_processes_unless_the_controller_section_says_docker() {
    use narsor_engine::config::local_controller::LocalMode;
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::CONTROLLER);
    assert_eq!(resolved.local_mode(), LocalMode::Process);
    assert_eq!(resolved.local_docker_update_interval(), Some(24 * 60 * 60));

    root.user_config(
        "controller:\n  local:\n    mode: docker\n    docker:\n      update_interval: 6h\n",
    );
    let resolved = root.resolve(
        &Flags::default(),
        &[("NARSOR_CONTROLLER__LOCAL__DOCKER__UPDATE_INTERVAL", "none")],
        Scope::CONTROLLER,
    );
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(resolved.local_mode(), LocalMode::Docker);
    assert!(matches!(
        resolved.source(SettingKey::LocalMode),
        Source::File(_)
    ));
    assert_eq!(resolved.local_docker_update_interval(), None);

    root.user_config("controller:\n  local:\n    mode: dokcer\n");
    let resolved = root.resolve(&Flags::default(), &[], Scope::CONTROLLER);
    let error = resolved.errors()[0].to_string();
    assert!(
        error.contains("controller.local.mode") && error.contains("process, docker"),
        "{error}"
    );
}

#[test]
fn the_observer_worker_is_off_unless_the_observer_group_enables_it() {
    use narsor_engine::config::ObserverWorkerConfig;
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(resolved.observer_worker(), None);

    root.user_config(
        "server:\n  observer:\n    enabled: true\n    server_url: https://narsor.example.com/\n    secrets_dir: /etc/narsor/observer-secrets\n    script_user: narsor-observer-script\n",
    );
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(
        resolved.observer_worker(),
        Some(ObserverWorkerConfig {
            server: Some("https://narsor.example.com".to_string()),
            secrets_dir: Some(PathBuf::from("/etc/narsor/observer-secrets")),
            script_user: Some("narsor-observer-script".to_string()),
            insecure_same_user: false,
        })
    );

    // The switches as the variables' text.
    let resolved = TempRoot::new().resolve(
        &Flags::default(),
        &[
            ("NARSOR_SERVER__OBSERVER__ENABLED", "true"),
            ("NARSOR_SERVER__OBSERVER__INSECURE_SAME_USER", "true"),
        ],
        Scope::SERVER,
    );
    assert!(resolved.observer_worker().unwrap().insecure_same_user);

    root.user_config("server:\n  observer:\n    enabled: on\n    secrets_dir: relative/dir\n");
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(resolved.errors().len(), 2, "{:?}", resolved.errors());
}

#[test]
fn agent_args_come_from_the_runner_section_or_its_variable_and_are_checked() {
    let root = TempRoot::new();
    root.user_config(
        "runner:\n  agent_args:\n    claude: [--dangerously-skip-permissions, --max-turns, 5, \"--append-system-prompt=a b\", \"--search=a,b\"]\n    codex: [--x]\n",
    );
    let resolved = root.resolve(&Flags::default(), &[], Scope::RUNNER);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    let args = resolved.agent_args();
    assert_eq!(
        args["claude"],
        [
            "--dangerously-skip-permissions",
            "--max-turns",
            "5",
            "--append-system-prompt=a b",
            "--search=a,b"
        ]
    );
    assert_eq!(args["codex"], ["--x"]);

    let env = [("NARSOR_RUNNER__AGENT_ARGS", "{claude-code: [--a, \"b c\"]}")];
    let resolved = root.resolve(&Flags::default(), &env, Scope::RUNNER);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(resolved.agent_args()["claude"], ["--a", "b c"]);

    for bad in ["[--a]", "{nobody: [--a]}", "{claude: --a}"] {
        let resolved = root.resolve(
            &Flags::default(),
            &[("NARSOR_RUNNER__AGENT_ARGS", bad)],
            Scope::RUNNER,
        );
        assert!(!resolved.is_ok(), "{bad}");
    }

    root.user_config("runner:\n  agent_args: [--a]\n");
    assert!(!root.resolve(&Flags::default(), &[], Scope::RUNNER).is_ok());
}

#[test]
fn the_task_limit_defaults_to_one_and_refuses_zero() {
    let root = TempRoot::new();
    assert_eq!(
        root.resolve(&Flags::default(), &[], Scope::SERVER)
            .max_concurrent_tasks(),
        Some(1)
    );

    root.user_config("server:\n  max_concurrent_tasks: 10\n");
    let resolved = root.resolve(&Flags::default(), &[], Scope::SERVER);
    assert_eq!(resolved.max_concurrent_tasks(), Some(10));

    root.user_config("server:\n  max_concurrent_tasks: 0\n");
    assert!(!root.resolve(&Flags::default(), &[], Scope::SERVER).is_ok());
}

#[test]
fn the_tui_refresh_interval_defaults_to_ten_seconds_and_the_environment_beats_the_file() {
    let root = TempRoot::new();
    let resolved = root.resolve(&Flags::default(), &[], Scope::TUI);
    assert_eq!(resolved.tui_refresh_interval(), Some(10));

    root.user_config("tui:\n  refresh_interval: 30s\n");
    let resolved = root.resolve(&Flags::default(), &[], Scope::CLI);
    assert_eq!(
        resolved.tui_refresh_interval(),
        Some(30),
        "narsor-cli reads the tui section too (R25)"
    );

    let resolved = root.resolve(
        &Flags::default(),
        &[("NARSOR_TUI__REFRESH_INTERVAL", "none")],
        Scope::TUI,
    );
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(resolved.tui_refresh_interval(), None, "none turns it off");
}
