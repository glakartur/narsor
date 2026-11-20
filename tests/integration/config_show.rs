//! `config show` merges the configuration checks of every installed binary
//! (R14, R15, R24; AE6).
//!
//! The other binaries' reports come through the `ConfigChecker` seam as
//! canned reports; the one `#[ignore]`d test runs the built binaries.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use narsor::cli::commands::{
    config_show_with, run_check, CheckOutcome, ConfigChecker, Sibling, CLI_BINARY,
};
use narsor::cli::ConfigFormat;
use narsor_engine::config::cascade::ConfigCascade;
use narsor_engine::config::check::{CheckError, CheckedSetting, CheckedSource};
use narsor_engine::config::resolve::{resolve_with, Environment, Flags};
use narsor_engine::config::{CheckReport, Scope};
use narsor_server::settings::configure;
use serde_json::Value;

/// A report of `binary` with `settings` (path, value) from the file, and
/// `errors` (path, message), each marking its setting.
fn report(binary: &str, settings: &[(&str, &str)], errors: &[(&str, &str)]) -> CheckReport {
    let mut entries = BTreeMap::new();
    for (path, value) in settings {
        entries.insert(
            path.to_string(),
            CheckedSetting {
                value: Some(value.to_string()),
                source: Some(CheckedSource {
                    kind: "file".into(),
                    detail: Some("/etc/narsor/config.yaml".into()),
                }),
                error: errors
                    .iter()
                    .find(|(error_path, _)| error_path == path)
                    .map(|(_, message)| message.to_string()),
                sensitive: false,
            },
        );
    }
    CheckReport {
        version: 2,
        binary: binary.into(),
        file: Some("/etc/narsor/config.yaml".into()),
        shadowed: Vec::new(),
        settings: entries,
        errors: errors
            .iter()
            .map(|(path, message)| CheckError {
                path: Some(path.to_string()),
                message: message.to_string(),
            })
            .collect(),
    }
}

/// The outcomes by binary; a binary not named is not installed.
struct Canned(BTreeMap<&'static str, CheckOutcome>);

impl Canned {
    fn new(outcomes: Vec<(&'static str, CheckOutcome)>) -> Self {
        Self(outcomes.into_iter().collect())
    }
}

impl ConfigChecker for Canned {
    fn check(&self, sibling: &Sibling) -> CheckOutcome {
        self.0
            .get(sibling.name)
            .cloned()
            .unwrap_or(CheckOutcome::NotInstalled)
    }
}

/// What `config show` printed in `format`, and its exit code.
fn show(own: CheckReport, checker: &Canned, format: ConfigFormat) -> (String, i32) {
    let mut out = Vec::new();
    let code = match config_show_with(own, checker, format, &mut out) {
        Ok(()) => 0,
        Err(error) => error.exit_code,
    };
    (String::from_utf8(out).expect("UTF-8 output"), code)
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("config show prints JSON")
}

fn status_of<'a>(document: &'a Value, binary: &str) -> &'a Value {
    document["binaries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == binary)
        .unwrap_or_else(|| panic!("{binary} missing from {document}"))
}

fn clean_cli() -> CheckReport {
    report(CLI_BINARY, &[("server.state", "/srv/narsor")], &[])
}

#[test]
fn errors_of_two_binaries_mark_both_sections_and_fail_the_command() {
    // AE6.
    let checker = Canned::new(vec![
        (
            "narsor-server",
            CheckOutcome::Report(report(
                "narsor-server",
                &[("server.mode", "network"), ("server.oidc.issuer", "")],
                &[("server.oidc.issuer", "server.oidc.issuer is not set")],
            )),
        ),
        (
            "narsor-web",
            CheckOutcome::Report(report(
                "narsor-web",
                &[("web.bind", "127.0.0.1:8484"), ("web.token", "passthrough")],
                &[(
                    "web.token",
                    "web.token: 'passthrough' with web.upstream: 'local'",
                )],
            )),
        ),
    ]);

    let (table, code) = show(clean_cli(), &checker, ConfigFormat::Table);
    assert_eq!(code, 1, "{table}");
    for expected in [
        "[server]",
        "server.mode",
        "error: server.oidc.issuer is not set",
        "[web]",
        "web.bind",
        "error: web.token: 'passthrough'",
        "narsor-server      errors: 1 problem(s)",
    ] {
        assert!(
            table.contains(expected),
            "{expected} missing from:\n{table}"
        );
    }

    let (text, code) = show(clean_cli(), &checker, ConfigFormat::Json);
    assert_eq!(code, 1);
    let document = json(&text);
    assert_eq!(document["version"], 2);
    assert_eq!(status_of(&document, "narsor-web")["status"], "errors");
    assert_eq!(status_of(&document, CLI_BINARY)["status"], "ok");
}

#[test]
fn a_binary_that_is_not_installed_is_listed_without_failing_the_command() {
    // R24: only narsor-web is installed beside narsor-cli.
    let checker = Canned::new(vec![(
        "narsor-web",
        CheckOutcome::Report(report("narsor-web", &[("web.bind", "127.0.0.1:8484")], &[])),
    )]);

    let (table, code) = show(clean_cli(), &checker, ConfigFormat::Table);
    assert_eq!(code, 0, "{table}");
    assert!(
        table.contains("narsor-runner      not installed"),
        "{table}"
    );
    assert!(
        table.contains("[runner] (narsor-runner: not installed)"),
        "{table}"
    );

    let (text, code) = show(clean_cli(), &checker, ConfigFormat::Json);
    assert_eq!(code, 0);
    assert_eq!(
        status_of(&json(&text), "narsor-server")["status"],
        "not installed"
    );
}

#[test]
fn a_binary_without_a_usable_report_failed_to_run_and_fails_the_command() {
    for (script, reason) in [
        ("echo 'not a report'", "not a check report"),
        ("exit 3", "ended with"),
    ] {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let outcome = run_check(command);
        assert!(
            matches!(&outcome, CheckOutcome::Failed(why) if why.contains(reason)),
            "{script}: {outcome:?}"
        );

        let checker = Canned::new(vec![("narsor-runner", outcome)]);
        let (text, code) = show(clean_cli(), &checker, ConfigFormat::Json);
        assert_eq!(code, 1, "{script}");
        let status = status_of(&json(&text), "narsor-runner").clone();
        assert_eq!(status["status"], "failed to run", "{script}");
        assert!(
            status["detail"].as_str().unwrap().contains(reason),
            "{status}"
        );
    }
}

#[test]
fn a_setting_read_by_several_binaries_with_one_value_is_one_row() {
    let checker = Canned::new(vec![(
        "narsor-web",
        CheckOutcome::Report(report(
            "narsor-web",
            &[("server.state", "/srv/narsor")],
            &[],
        )),
    )]);

    let (text, _) = show(clean_cli(), &checker, ConfigFormat::Json);
    let document = json(&text);
    let rows: Vec<&Value> = document["settings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["path"] == "server.state")
        .collect();
    assert_eq!(rows.len(), 1, "{document}");
    assert_eq!(
        rows[0]["binaries"],
        serde_json::json!([CLI_BINARY, "narsor-web"])
    );
}

#[test]
fn a_sensitive_value_never_reaches_the_output() {
    // Masking is the producer's job; `config show` masks again, so a binary
    // that sent a raw value still does not get it printed.
    let mut web = report("narsor-web", &[("web.api_key", "s3cret-value")], &[]);
    web.settings.get_mut("web.api_key").unwrap().sensitive = true;
    let checker = Canned::new(vec![("narsor-web", CheckOutcome::Report(web))]);

    for format in [ConfigFormat::Table, ConfigFormat::Json] {
        let (text, _) = show(clean_cli(), &checker, format);
        assert!(text.contains("web.api_key"), "{text}");
        assert!(!text.contains("s3cret-value"), "{text}");
    }
}

/// A configuration file in a temporary directory that deletes itself.
struct ConfigFile {
    dir: PathBuf,
}

impl ConfigFile {
    fn new(contents: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("narsor-config-show-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create the directory");
        fs::write(dir.join("config.yaml"), contents).expect("write the file");
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("config.yaml")
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_server_check_marks_a_rule_problem_on_its_setting_and_keeps_the_good_values() {
    let file = ConfigFile::new("server:\n  mode: network\n  oidc:\n    audience: narsor\n");
    let environment = Environment::new();
    let cascade = ConfigCascade::with_dirs(Some(file.path()), None, None);
    let resolved = resolve_with(&Flags::default(), &environment, &cascade, Scope::SERVER);
    let problems = configure(&resolved, &environment).expect_err("no issuer");

    let report = CheckReport::build("narsor-server", &resolved, &problems);

    assert_eq!(report.exit_code(), 1);
    assert!(report
        .errors
        .iter()
        .any(|error| error.path.as_deref() == Some("server.oidc.issuer")));
    assert!(report.settings["server.oidc.issuer"].error.is_some());
    let audience = &report.settings["server.oidc.audience"];
    assert_eq!(audience.value.as_deref(), Some("narsor"));
    assert_eq!(audience.error, None);
    assert_eq!(CheckReport::from_json(&report.to_json()), Ok(report));
}

#[test]
#[ignore = "runs the built narsor-server and narsor-cli binaries"]
fn the_built_server_check_reports_a_missing_issuer_without_starting() {
    let file = ConfigFile::new("server:\n  mode: network\n");
    let root = file.dir.join("root");

    // `narsor-cli` is a binary of this package; the workspace's other
    // binaries are built next to it.
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_narsor-cli"));
    let server = cli.with_file_name("narsor-server");
    assert!(server.is_file(), "build the workspace binaries first");

    let output = Command::new(server)
        .arg("--check-config")
        .arg("--config")
        .arg(file.path())
        .arg("--root")
        .arg(&root)
        .output()
        .expect("run narsor-server");

    assert_eq!(output.status.code(), Some(1));
    let report = CheckReport::from_json(&String::from_utf8_lossy(&output.stdout))
        .expect("a check report on stdout");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path.as_deref() == Some("server.oidc.issuer")),
        "{report:?}"
    );
    assert!(!root.exists(), "the check started the server");

    let output = Command::new(cli)
        .args(["config", "show", "--format", "json", "--config"])
        .arg(file.path())
        .output()
        .expect("run narsor-cli");
    assert_eq!(output.status.code(), Some(1));
    let document = json(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(status_of(&document, "narsor-server")["status"], "errors");
    assert_eq!(status_of(&document, "narsor-web")["status"], "ok");
}
