//! Layering a value out of the flag, the environment, the file and the
//! built-in (U5).
//!
//! # Not one call to `set_var`
//!
//! The environment is built as a map and handed to
//! [`resolve_with`](narsor::config::resolve::resolve_with), and the cascade is
//! given both of its directories explicitly. Nothing here reads the process
//! environment, so nothing here can race a test running beside it in the same
//! binary — which is what `std::env::set_var` would guarantee it does.

use std::fs;
use std::path::{Path, PathBuf};

use narsor::config::cascade::ConfigCascade;
use narsor::config::resolve::{resolve_with, Environment, Flags};
use narsor::config::{SettingKey, SettingValue, Source};

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

#[test]
fn a_flag_beats_the_file_and_says_so() {
    // AE1: the operator passed --root, so the answer to "where did this come
    // from" is the flag, not the file that also mentions it.
    let root = TempRoot::new();
    root.user_config("state_dir: /from/the/file\n");

    let flags = Flags {
        root: Some(PathBuf::from("/from/the/flag")),
        config: None,
    };
    let resolved = resolve_with(&flags, &environment(&[]), &cascade(&root));

    assert!(resolved.is_ok());
    assert_eq!(resolved.state_dir(), Path::new("/from/the/flag"));
    assert_eq!(
        resolved.source(SettingKey::StateDir),
        &Source::Flag("--root")
    );
}

#[test]
fn the_environment_beats_the_file_and_loses_to_the_flag() {
    let root = TempRoot::new();
    root.user_config("state_dir: /from/the/file\n");
    let env = environment(&[("NARSOR_STATE_DIR", "/from/the/environment")]);

    let resolved = resolve_with(&Flags::default(), &env, &cascade(&root));
    assert_eq!(resolved.state_dir(), Path::new("/from/the/environment"));
    assert_eq!(
        resolved.source(SettingKey::StateDir),
        &Source::Env("NARSOR_STATE_DIR".to_string()),
        "the source names the variable, not the word 'environment'"
    );

    let flags = Flags {
        root: Some(PathBuf::from("/from/the/flag")),
        config: None,
    };
    let resolved = resolve_with(&flags, &env, &cascade(&root));
    assert_eq!(resolved.state_dir(), Path::new("/from/the/flag"));
}

#[test]
fn the_file_beats_the_built_in_and_names_its_own_path() {
    // R8: the path of the file that was actually read.
    let root = TempRoot::new();
    let path = root.user_config("state_dir: /from/the/file\n");

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade(&root));

    assert_eq!(resolved.state_dir(), Path::new("/from/the/file"));
    assert_eq!(resolved.source(SettingKey::StateDir), &Source::File(path));
}

#[test]
fn with_nothing_anywhere_every_setting_is_built_in_and_nothing_fails() {
    // AE2: no file, no home, no XDG — and a working command.
    let resolved = resolve_with(
        &Flags::default(),
        &environment(&[]),
        &ConfigCascade::with_dirs(None, None, None),
    );

    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    for setting in SettingKey::ALL {
        assert_eq!(
            resolved.source(setting),
            &Source::BuiltIn,
            "{setting} has nothing else to come from"
        );
        assert_eq!(resolved.value(setting), &setting.built_in());
    }
    assert_eq!(resolved.state_dir(), Path::new("./.narsor"));
    assert_eq!(resolved.agent_harness(), "claude-code");
    assert_eq!(resolved.agent_timeout(), None);
}

#[test]
fn an_empty_environment_variable_counts_as_unset() {
    // D-J: that is how Kubernetes renders an unset key of a ConfigMap.
    let root = TempRoot::new();
    let path = root.user_config("state_dir: /from/the/file\n");
    let env = environment(&[("NARSOR_DATA_DIR", "")]);

    let resolved = resolve_with(&Flags::default(), &env, &cascade(&root));

    assert_eq!(resolved.state_dir(), Path::new("/from/the/file"));
    assert_eq!(resolved.source(SettingKey::StateDir), &Source::File(path));
}

#[test]
fn rust_log_wins_over_the_setting_and_is_the_source_reported() {
    // D-K: RUST_LOG takes per-target directives, so the sharper instrument
    // wins — and `config show` names the variable that actually took effect.
    let env = environment(&[
        ("RUST_LOG", "narsor::engine=debug"),
        ("NARSOR_LOG_LEVEL", "warn"),
    ]);

    let resolved = resolve_with(
        &Flags::default(),
        &env,
        &ConfigCascade::with_dirs(None, None, None),
    );

    assert_eq!(
        resolved.value(SettingKey::LogLevel),
        &SettingValue::Text("narsor::engine=debug".to_string())
    );
    assert_eq!(
        resolved.source(SettingKey::LogLevel),
        &Source::Env("RUST_LOG".to_string())
    );
}

#[test]
fn without_rust_log_the_setting_decides() {
    let env = environment(&[("NARSOR_LOG_LEVEL", "warn")]);

    let resolved = resolve_with(
        &Flags::default(),
        &env,
        &ConfigCascade::with_dirs(None, None, None),
    );

    assert_eq!(resolved.log_directive(), Some("warn".to_string()));
    assert_eq!(
        resolved.source(SettingKey::LogLevel),
        &Source::Env("NARSOR_LOG_LEVEL".to_string())
    );
}

#[test]
fn nothing_configured_installs_no_logging_at_all() {
    // D-N: today an unset RUST_LOG leaves the process without a subscriber and
    // every subcommand silent. The built-in level keeps that exactly.
    let resolved = resolve_with(
        &Flags::default(),
        &environment(&[]),
        &ConfigCascade::with_dirs(None, None, None),
    );

    assert_eq!(
        resolved.log_directive(),
        None,
        "no directive means no subscriber, which means no output"
    );
}

#[test]
fn a_level_from_the_file_turns_logging_on() {
    let root = TempRoot::new();
    root.user_config("log_level: debug\n");

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade(&root));

    assert_eq!(resolved.log_directive(), Some("debug".to_string()));
}

#[test]
fn settings_are_resolved_one_by_one_not_file_by_file() {
    // R2: a file that sets one key does not displace a flag that sets another.
    let root = TempRoot::new();
    let path = root.user_config("state_dir: /from/the/file\nagent_harness: opencode\n");

    let flags = Flags {
        root: Some(PathBuf::from("/from/the/flag")),
        config: None,
    };
    let resolved = resolve_with(&flags, &environment(&[]), &cascade(&root));

    assert_eq!(
        resolved.source(SettingKey::StateDir),
        &Source::Flag("--root")
    );
    assert_eq!(
        resolved.source(SettingKey::AgentHarness),
        &Source::File(path),
        "the harness still comes from the file the flag had nothing to say about"
    );
    assert_eq!(resolved.agent_harness(), "opencode");
    assert_eq!(
        resolved.source(SettingKey::LogLevel),
        &Source::BuiltIn,
        "and a key nobody mentioned is still built in"
    );
}

#[test]
fn the_cascade_looks_for_the_file_regardless_of_the_three_roots() {
    // D-A: the data directory is an ordinary setting. Pointing it somewhere
    // else does not move the file, and the file is still what sets it.
    let root = TempRoot::new();
    let path = root.user_config("state_dir: /somewhere/else\n");

    let flags = Flags {
        root: None,
        config: None,
    };
    let resolved = resolve_with(&flags, &environment(&[]), &cascade(&root));

    assert_eq!(resolved.state_dir(), Path::new("/somewhere/else"));
    assert_eq!(resolved.file().path(), Some(path.as_path()));
}

#[test]
fn a_broken_file_yields_errors_and_still_yields_every_value() {
    // D-D: resolution never returns a Result, because the decision to refuse
    // to start belongs to the caller — `config show` makes the other one.
    let root = TempRoot::new();
    let path = root.user_config("nonsense: 1\nlog_level: shouting\n");

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade(&root));

    assert!(!resolved.is_ok());
    assert_eq!(resolved.errors().len(), 2);
    assert!(!resolved.has_usage_error(), "this one is exit code 1");
    for error in resolved.errors() {
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    // Everything the file did not break is still answerable.
    assert_eq!(resolved.state_dir(), Path::new("./.narsor"));
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

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade);

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
    let env = environment(&[("NARSOR_AGENT_TIMEOUT", "tomorrow")]);

    let resolved = resolve_with(
        &Flags::default(),
        &env,
        &ConfigCascade::with_dirs(None, None, None),
    );

    assert!(!resolved.is_ok());
    let message = resolved.errors()[0].to_string();
    assert!(message.contains("NARSOR_AGENT_TIMEOUT"), "{message}");
    assert!(message.contains("tomorrow"), "{message}");
}

#[test]
fn a_timeout_of_none_resolves_to_no_limit() {
    let root = TempRoot::new();
    root.user_config("agent_timeout: none\n");

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade(&root));

    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(resolved.agent_timeout(), None);
}

#[test]
fn resolving_writes_nothing_and_needs_no_writable_home() {
    // R9.
    let root = TempRoot::new();
    let user = root.user_dir();

    let resolved = resolve_with(&Flags::default(), &environment(&[]), &cascade(&root));

    assert!(resolved.is_ok());
    assert!(!user.exists(), "nothing was created to resolve a setting");
}
