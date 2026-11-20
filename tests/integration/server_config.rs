//! The server's configuration step: every rule between its settings, the
//! local controller's configuration it depends on, and what it hands that
//! controller (R7, R8, R21, R26).
//!
//! The environment is a map handed in, never the process's own.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::config::cascade::ConfigCascade;
use narsor_engine::config::resolve::{resolve_with, Environment, Flags, Resolved};
use narsor_engine::config::{legacy, local_controller, Scope, SettingKey, Source};
use narsor_engine::runner::HomeMode;
use narsor_server::child_process::variables_to_remove;
use narsor_server::settings::configure;

/// A configuration file in a temporary directory that deletes itself.
struct ConfigFile {
    dir: PathBuf,
}

impl ConfigFile {
    fn new(contents: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("narsor-server-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create the directory");
        fs::write(dir.join("config.yaml"), contents).expect("write the file");
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }

    fn resolve(&self, scope: Scope, env: &Environment) -> Resolved {
        let cascade = ConfigCascade::with_dirs(Some(self.path()), None, None);
        resolve_with(&Flags::default(), env, &cascade, scope)
    }

    /// The server's problems, empty when it would start.
    fn server_problems(&self, env: &[(&str, &str)]) -> Vec<String> {
        let env = environment(env);
        match configure(&self.resolve(Scope::SERVER, &env), &env) {
            Ok(_) => Vec::new(),
            Err(problems) => problems.iter().map(ToString::to_string).collect(),
        }
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn environment(pairs: &[(&str, &str)]) -> Environment {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn assert_names(problems: &[String], paths: &[&str]) {
    let all = problems.join("\n");
    for path in paths {
        assert!(all.contains(path), "{path} missing from:\n{all}");
    }
}

#[test]
fn network_mode_names_every_missing_setting_by_its_path_at_once() {
    // AE1.
    let network = ConfigFile::new("server:\n  mode: network\n");
    assert_names(
        &network.server_problems(&[]),
        &[
            "server.oidc.issuer",
            "server.oidc.audience",
            "server.tls.termination",
            "server.tls.trusted_proxies",
        ],
    );

    let trusted = ConfigFile::new("server:\n  mode: trusted\n");
    assert_eq!(trusted.server_problems(&[]), Vec::<String>::new());
}

#[test]
fn an_observer_worker_that_could_not_run_is_refused_before_anything_starts() {
    // AE2, and network mode's need for a URL.
    for (observer, mode, expected) in [
        (
            "script_user: scripts\n    insecure_same_user: true",
            "trusted",
            "contradict",
        ),
        (
            "secrets_dir: /etc/secrets",
            "trusted",
            "server.observer.script_user",
        ),
        (
            "script_user: scripts",
            "network",
            "server.observer.server_url",
        ),
    ] {
        let file = ConfigFile::new(&format!(
            "server:\n  mode: {mode}\n  observer:\n    enabled: true\n    {observer}\n"
        ));
        let problems = file.server_problems(&[]);
        assert_names(&problems, &[expected]);
    }
}

#[test]
fn a_broken_section_stops_its_own_binary_and_the_server_that_starts_its_controller() {
    // AE3: the CLI and the runner do not read either section; the server
    // reads its own and checks the controller's before it binds (R21).
    let file =
        ConfigFile::new("server:\n  mode: netwrk\ncontroller:\n  local:\n    home: everywhere\n");
    for scope in [Scope::CLI, Scope::RUNNER, Scope::WEB] {
        let resolved = file.resolve(scope, &Environment::new());
        assert!(resolved.is_ok(), "{scope:?}: {:?}", resolved.errors());
    }
    assert_names(
        &file.server_problems(&[]),
        &["server.mode", "controller.local.home"],
    );

    // The controller's variables count, since the controller gets them.
    let clean = ConfigFile::new("server:\n  mode: trusted\n");
    assert_names(
        &clean.server_problems(&[("NARSOR_CONTROLLER__LOCAL__HOME", "everywhere")]),
        &["NARSOR_CONTROLLER__LOCAL__HOME"],
    );
}

#[test]
fn the_controller_gets_the_winning_file_and_only_its_own_sections_variables() {
    let prefixes = local_controller::forwarded_prefixes();
    let names = [
        "NARSOR_CONTROLLER__LOCAL__HOME",
        "NARSOR_GLOBAL__LOG_LEVEL",
        "NARSOR_WEB__BIND",
        "NARSOR_TOKEN",
        "NARSOR_CONTROLLER_CREDENTIAL_FILE",
        "PATH",
    ]
    .map(OsString::from);
    assert_eq!(
        variables_to_remove(names, &prefixes),
        [
            "NARSOR_WEB__BIND",
            "NARSOR_TOKEN",
            "NARSOR_CONTROLLER_CREDENTIAL_FILE"
        ]
        .map(OsString::from)
    );

    // What the controller then reads from a server with no file and
    // NARSOR_CONTROLLER__LOCAL__HOME=private.
    let controller = local_controller::resolve(
        None,
        &environment(&[
            ("NARSOR_CONTROLLER__LOCAL__HOME", "private"),
            ("NARSOR_WEB__BIND", "bad"),
        ]),
    );
    assert!(controller.is_ok(), "{:?}", controller.errors());
    assert_eq!(controller.local_harness_home().mode, HomeMode::Private);
    assert_eq!(
        controller.source(SettingKey::LocalHome),
        &Source::Env("NARSOR_CONTROLLER__LOCAL__HOME".to_string())
    );

    assert_eq!(
        local_controller::config_args(Some(Path::new("/etc/narsor/config.yaml"))),
        ["--config", "/etc/narsor/config.yaml"].map(OsString::from)
    );
    assert_eq!(
        local_controller::config_args(None),
        [OsString::from("--no-config")]
    );
}

#[test]
fn old_flat_names_are_ignored_and_named_in_one_warning() {
    // R26, with AE4/AE7 still true: nothing is refused.
    let file = ConfigFile::new("server_mode: network\nserver:\n  bind: 127.0.0.1:0\n");
    let env = environment(&[("NARSOR_OIDC_ISSUER", "https://issuer.example")]);
    let resolved = file.resolve(Scope::SERVER, &env);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    assert_eq!(resolved.text(SettingKey::ServerMode), Some("trusted"));

    let warning = legacy::warning(&resolved, &env).expect("a warning");
    for named in [
        "server_mode",
        "server.mode",
        "NARSOR_OIDC_ISSUER",
        "NARSOR_SERVER__OIDC__ISSUER",
    ] {
        assert!(warning.contains(named), "{named} missing from: {warning}");
    }

    let migrated = ConfigFile::new("server:\n  mode: trusted\n");
    let env = environment(&[("NARSOR_SERVER__MODE", "trusted"), ("NARSOR_TOKEN", "t")]);
    assert_eq!(
        legacy::warning(&migrated.resolve(Scope::SERVER, &env), &env),
        None
    );
}
