use serde_yaml::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "narsor-connection-profile-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create test root");
        Self(path)
    }

    fn config_home(&self) -> PathBuf {
        self.0.join("config")
    }

    fn profile_path(&self) -> PathBuf {
        self.config_home().join("narsor/connection.yaml")
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        self.command(args, environment)
            .output()
            .expect("run narsor-cli")
    }

    fn run_without_stdin(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        self.command(args, environment)
            .stdin(Stdio::null())
            .output()
            .expect("run narsor-cli")
    }

    fn command(&self, args: &[&str], environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_narsor-cli"));
        command
            .arg("--root")
            .arg(self.0.join("state"))
            .args(args)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.config_home())
            .env_remove("NARSOR_SERVER")
            .env_remove("NARSOR_TOKEN")
            .env_remove("NARSOR_TOKEN_FILE")
            .env_remove("NARSOR_TOKEN_COMMAND");
        for (key, value) in environment {
            command.env(key, value);
        }
        command
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

static NEXT_ROOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn write_profile(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("profile parent")).expect("create profile directory");
    fs::write(path, contents).expect("write profile fixture");
}

fn run_import(root: &TestRoot, source: &Path, trust: bool) -> Output {
    let source = source.to_str().expect("profile path");
    let mut args = vec!["connection-profile", "import", source];
    if trust {
        args.push("--trust-server");
    }
    root.run_without_stdin(&args, &[])
}

#[test]
fn import_requires_explicit_origin_trust_before_writing() {
    let root = TestRoot::new();
    let source = root.0.join("incoming.yaml");
    write_profile(&source, "server_url: https://backend.example.test/api/\n");

    let result = run_import(&root, &source, false);

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("https://backend.example.test"), "{stderr}");
    assert!(stderr.contains("--trust-server"), "{stderr}");
    assert!(!root.profile_path().exists());
}

#[test]
fn importing_and_exporting_preserves_only_the_nonsecret_endpoint() {
    let root = TestRoot::new();
    let source = root.0.join("incoming.yaml");
    write_profile(
        &source,
        "server_url: https://BACKEND.example.test:443/narsor/\n",
    );

    let imported = run_import(&root, &source, true);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert!(
        String::from_utf8_lossy(&imported.stderr)
            .contains("Origin to trust: https://backend.example.test"),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let stored = fs::read_to_string(root.profile_path()).expect("read imported profile");
    let stored: Value = serde_yaml::from_str(&stored).expect("parse imported profile");
    assert_eq!(stored["server_url"], "https://backend.example.test/narsor");
    assert!(stored.get("token").is_none());
    assert!(stored.get("token_file").is_none());

    let token_file = root.0.join("access-token");
    fs::write(&token_file, "local-token-source").expect("write token source");
    let exported_path = root.0.join("exported.yaml");
    let exported = root.run(
        &[
            "--token-file",
            token_file.to_str().expect("token file path"),
            "--server",
            "https://backend.example.test/narsor",
            "connection-profile",
            "export",
            "--output",
            exported_path.to_str().expect("export path"),
        ],
        &[],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let exported = fs::read_to_string(exported_path).expect("read exported profile");
    assert!(exported.contains("https://backend.example.test/narsor"));
    assert!(!exported.contains("local-token-source"));
    assert!(token_file.is_file());
}

#[test]
fn explicit_and_environment_servers_precede_the_imported_profile() {
    let root = TestRoot::new();
    let source = root.0.join("incoming.yaml");
    write_profile(&source, "server_url: https://imported.example.test\n");
    let imported = run_import(&root, &source, true);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let remote_target = |args: &[&str], environment: &[(&str, &str)]| {
        let mut command_args = args.to_vec();
        command_args.extend(["repo", "list"]);
        let result = root.run(&command_args, environment);
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
        assert!(stderr.contains("no token for"), "{stderr}");
        stderr
    };

    assert!(remote_target(&[], &[]).contains("https://imported.example.test"));
    assert!(remote_target(
        &[],
        &[("NARSOR_SERVER", "https://environment.example.test")]
    )
    .contains("https://environment.example.test"));
    assert!(remote_target(
        &["--server", "https://flag.example.test"],
        &[("NARSOR_SERVER", "https://environment.example.test")]
    )
    .contains("https://flag.example.test"));
}

#[test]
fn tui_and_cli_use_the_imported_endpoint_when_no_server_is_supplied() {
    let root = TestRoot::new();
    let source = root.0.join("incoming.yaml");
    write_profile(&source, "server_url: https://backend.example.test\n");
    let imported = run_import(&root, &source, true);
    assert!(imported.status.success());

    let cli = root.run(&["repo", "list"], &[]);
    let cli_error = String::from_utf8_lossy(&cli.stderr);
    assert!(
        cli_error.contains("https://backend.example.test"),
        "{cli_error}"
    );

    let mut tui = Command::new(env!("CARGO_BIN_EXE_narsor"));
    tui.arg("--root")
        .arg(root.0.join("state"))
        .env("HOME", &root.0)
        .env("XDG_CONFIG_HOME", root.config_home())
        .env_remove("NARSOR_SERVER")
        .env_remove("NARSOR_TOKEN")
        .env_remove("NARSOR_TOKEN_FILE")
        .env_remove("NARSOR_TOKEN_COMMAND");
    let tui = tui.output().expect("run narsor TUI");
    let tui_error = String::from_utf8_lossy(&tui.stderr);
    assert!(
        tui_error.contains("https://backend.example.test"),
        "{tui_error}"
    );
}

#[test]
fn malformed_or_insecure_profiles_do_not_replace_the_imported_endpoint() {
    let root = TestRoot::new();
    let existing = root.0.join("existing.yaml");
    write_profile(&existing, "server_url: https://trusted.example.test\n");
    let imported = run_import(&root, &existing, true);
    assert!(imported.status.success());
    let before = fs::read(root.profile_path()).expect("read current profile");

    for contents in [
        "server_url: http://backend.example.test\n",
        "server_url: https://backend.example.test\naccess_token: secret\n",
    ] {
        let source = root.0.join("invalid.yaml");
        write_profile(&source, contents);
        let result = run_import(&root, &source, true);
        assert!(!result.status.success());
        assert_eq!(
            fs::read(root.profile_path()).expect("read current profile"),
            before
        );
    }
}
