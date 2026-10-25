//! The `narsor-controller` command line (U1): one subcommand per backend,
//! the shared connection arguments, and an enrollment code that never
//! travels on the command line (R1-R4, R12; AE1, AE2).

mod engine_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use narsor_controller::controller::ConnectionArgs;

/// The controller's variables, none of which a test inherits by accident.
const CONTROLLER_ENVS: [&str; 7] = [
    "NARSOR_CONTROL_PLANE",
    "NARSOR_RUNNER_CONTROL_PLANE",
    "NARSOR_CONTROLLER_CREDENTIAL_FILE",
    "NARSOR_ENROLLMENT_CODE",
    "NARSOR_ENROLLMENT_CODE_FILE",
    "NARSOR_KUBERNETES_CONFIG",
    "RUST_LOG",
];

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "narsor-controller-cli-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create the temporary directory");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn controller(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(engine_support::workspace_binary("narsor-controller"));
    for name in CONTROLLER_ENVS {
        command.env_remove(name);
    }
    command
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .expect("run narsor-controller")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Covers AE1.
#[test]
fn without_a_subcommand_it_is_a_usage_error_naming_both_backends() {
    let output = controller(&[], &[]);
    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(text.contains("kubernetes"), "{text}");
    assert!(text.contains("local"), "{text}");
}

#[test]
fn the_main_help_lists_both_backends() {
    let output = controller(&["--help"], &[]);
    assert!(output.status.success());
    let help = stdout(&output);
    assert!(help.contains("kubernetes"), "{help}");
    assert!(help.contains("local"), "{help}");
}

/// Covers AE2.
#[test]
fn edit_claude_profile_needs_no_connection_arguments() {
    let output = controller(&["kubernetes", "edit-claude-profile", "--help"], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("--control-plane"));

    let dir = TempDir::new();
    let config = dir.path().join("missing.yaml");
    let output = controller(
        &[
            "kubernetes",
            "edit-claude-profile",
            "--kubernetes-config",
            config.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(text.contains("cannot read"), "{text}");
    assert!(!text.contains("--control-plane"), "{text}");
    assert!(!text.contains("--credential-file"), "{text}");
}

#[test]
fn the_kubernetes_help_names_its_arguments_and_none_of_local() {
    let output = controller(&["kubernetes", "--help"], &[]);
    assert!(output.status.success());
    let help = stdout(&output);
    for flag in [
        "--kubernetes-config",
        "--control-plane",
        "--credential-file",
        "--enrollment-code-file",
        "edit-claude-profile",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
    assert!(!help.contains("--storage"), "{help}");
    assert!(!help.contains("--enrollment-code <"), "{help}");
}

#[test]
fn the_local_help_names_its_arguments() {
    let output = controller(&["local", "--help"], &[]);
    assert!(output.status.success());
    let help = stdout(&output);
    for flag in ["--storage", "--home", "--control-plane", "--lock-timeout-secs"] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
    assert!(!help.contains("--kubernetes-config"), "{help}");
    assert!(!help.contains("--parent-pid"), "{help}");
}

#[test]
fn the_connection_comes_from_the_environment_alone() {
    let dir = TempDir::new();
    let config = dir.path().join("missing.yaml");
    let credential = dir.path().join("credential");
    let output = controller(
        &["kubernetes"],
        &[
            ("NARSOR_CONTROL_PLANE", "http://127.0.0.1:1"),
            ("NARSOR_CONTROLLER_CREDENTIAL_FILE", credential.to_str().unwrap()),
            ("NARSOR_ENROLLMENT_CODE", "code"),
            ("NARSOR_KUBERNETES_CONFIG", config.to_str().unwrap()),
        ],
    );
    // Past every argument check: what fails is reading the configuration.
    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(text.contains("cannot read"), "{text}");
    assert!(text.contains("missing.yaml"), "{text}");
}

#[test]
fn kubernetes_without_its_configuration_names_the_argument() {
    let output = controller(
        &["kubernetes"],
        &[
            ("NARSOR_CONTROL_PLANE", "http://127.0.0.1:1"),
            ("NARSOR_CONTROLLER_CREDENTIAL_FILE", "/nonexistent/credential"),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--kubernetes-config"));
}

#[test]
fn without_a_control_plane_it_names_the_argument() {
    let output = controller(
        &["kubernetes", "--credential-file", "/nonexistent/credential"],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let text = stderr(&output);
    assert!(text.contains("--control-plane"), "{text}");
    assert!(text.contains("NARSOR_CONTROL_PLANE"), "{text}");
}

#[test]
fn an_enrollment_code_on_the_command_line_is_refused_without_repeating_it() {
    for backend in ["kubernetes", "local"] {
        let output = controller(
            &[
                backend,
                "--storage",
                "/nonexistent",
                "--enrollment-code",
                "abc-secret",
            ]
            .into_iter()
            .filter(|arg| backend == "local" || !matches!(*arg, "--storage" | "/nonexistent"))
            .collect::<Vec<_>>(),
            &[("NARSOR_CONTROL_PLANE", "http://127.0.0.1:1")],
        );
        assert_eq!(output.status.code(), Some(2), "{backend}");
        let text = stderr(&output);
        assert!(text.contains("NARSOR_ENROLLMENT_CODE"), "{text}");
        assert!(text.contains("--enrollment-code-file"), "{text}");
        assert!(!text.contains("abc-secret"), "{text}");
    }
}

#[test]
fn an_enrollment_code_in_the_environment_never_shows() {
    let secret = "sekret-code-1234";
    let output = controller(
        &["kubernetes", "--help"],
        &[("NARSOR_ENROLLMENT_CODE", secret)],
    );
    assert!(!stdout(&output).contains(secret));

    let dir = TempDir::new();
    let credential = dir.path().join("state/credential");
    let output = controller(
        &[
            "local",
            "--storage",
            dir.path().join("storage").to_str().unwrap(),
            "--control-plane",
            "http://127.0.0.1:1",
            "--credential-file",
            credential.to_str().unwrap(),
        ],
        &[("NARSOR_ENROLLMENT_CODE", secret), ("RUST_LOG", "debug")],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("enrollment failed"), "{text}");
    assert!(!text.contains(secret), "{text}");
    assert!(!stdout(&output).contains(secret));
}

#[test]
fn the_controller_logs_to_stderr_under_rust_log() {
    let dir = TempDir::new();
    let output = controller(
        &[
            "local",
            "--storage",
            dir.path().join("storage").to_str().unwrap(),
            "--control-plane",
            "http://127.0.0.1:1",
            "--credential-file",
            dir.path().join("state/credential").to_str().unwrap(),
        ],
        &[("NARSOR_ENROLLMENT_CODE", "code"), ("RUST_LOG", "info")],
    );
    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.contains("INFO") && text.contains("connecting to the control plane"),
        "{text}"
    );
}

#[test]
fn the_enrollment_code_file_is_read_without_its_line_end() {
    let dir = TempDir::new();
    let file = dir.path().join("code");
    std::fs::write(&file, "the-code\n").unwrap();
    let args = ConnectionArgs {
        control_plane: Some("http://127.0.0.1:1".into()),
        credential_file: Some(dir.path().join("credential")),
        enrollment_code_file: Some(file),
        lease_ttl_secs: 60,
        ..ConnectionArgs::default()
    };
    let connection = args.resolve().expect("a connection");
    assert_eq!(connection.enrollment_code.as_deref(), Some("the-code"));
    assert_eq!(connection.runner_control_plane, "http://127.0.0.1:1");
    assert!(!format!("{connection:?}").contains("the-code"));
}

#[test]
fn an_empty_enrollment_code_file_is_a_usage_error() {
    let dir = TempDir::new();
    let file = dir.path().join("code");
    std::fs::write(&file, "\n").unwrap();
    let args = ConnectionArgs {
        control_plane: Some("http://127.0.0.1:1".into()),
        credential_file: Some(dir.path().join("credential")),
        enrollment_code_file: Some(file),
        ..ConnectionArgs::default()
    };
    let error = args.resolve().expect_err("no code");
    assert_eq!(error.code, 2);
}
