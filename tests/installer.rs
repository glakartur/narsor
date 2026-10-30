use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const LINUX_ARCHIVE: &str = "narsor-linux-x86_64-gnu.tar.gz";
const RELEASE_BINARIES: &[&str] = &[
    "narsor",
    "narsor-cli",
    "narsor-server",
    "narsor-controller",
    "narsor-runner",
];

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    bin: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("narsor-installer-{}", uuid::Uuid::new_v4()));
        let source = root.join("release");
        let bin = root.join("bin");
        let config = root.join("config/narsor/config.yaml");
        let data = root.join("data");
        fs::create_dir_all(&source).expect("create release directory");

        for name in [
            "narsor",
            "narsor-cli",
            "narsor-server",
            "narsor-controller",
            "narsor-runner",
        ] {
            let binary = source.join(name);
            fs::write(&binary, format!("#!/bin/sh\n# {name}\n")).expect("write fixture binary");
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make fixture binary executable");
        }

        Self {
            root,
            source,
            bin,
            config,
            data,
        }
    }

    fn run(&self, role: &str, extra: &[&str]) -> Output {
        self.run_with_overrides(role, extra, None, None)
    }

    fn run_with_overrides(
        &self,
        role: &str,
        extra: &[&str],
        catalog_dir: Option<&Path>,
        server_bind: Option<&str>,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_narsor-cli"));
        command
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env_remove("NARSOR_SERVER")
            .env_remove("NARSOR_CATALOG_DIR")
            .env_remove("NARSOR_STATE_DIR")
            .env_remove("NARSOR_WORK_DIR")
            .env_remove("NARSOR_SERVER_MODE")
            .env_remove("NARSOR_SERVER_BIND")
            .env_remove("NARSOR_OIDC_ISSUER")
            .env_remove("NARSOR_OIDC_AUDIENCE")
            .env_remove("NARSOR_TLS_TERMINATION")
            .env_remove("NARSOR_TRUSTED_PROXIES")
            .args([
                "install",
                "--role",
                role,
                "--non-interactive",
                "--source-dir",
                self.source.to_str().expect("source path"),
                "--bin-dir",
                self.bin.to_str().expect("binary path"),
                "--config-file",
                self.config.to_str().expect("config path"),
                "--data-dir",
                self.data.to_str().expect("data path"),
            ])
            .args(extra);
        if let Some(catalog_dir) = catalog_dir {
            command.env("NARSOR_CATALOG_DIR", catalog_dir);
        }
        if let Some(server_bind) = server_bind {
            command.env("NARSOR_SERVER_BIND", server_bind);
        }
        command.output().expect("run installer")
    }

    fn installed(&self) -> Vec<String> {
        let mut names = fs::read_dir(&self.bin)
            .expect("read binary directory")
            .map(|entry| {
                entry
                    .expect("read binary entry")
                    .file_name()
                    .to_string_lossy()
                    .into()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn local_role_installs_all_binaries_and_preserves_existing_settings() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.config.parent().expect("config parent")).unwrap();
    let original = "log_level: debug\n";
    fs::write(&fixture.config, original).unwrap();

    let result = fixture.run("local", &[]);

    assert_success(&result);
    assert_eq!(
        fixture.installed(),
        [
            "narsor",
            "narsor-cli",
            "narsor-controller",
            "narsor-runner",
            "narsor-server",
        ]
    );
    let config = read_yaml(&fixture.config);
    assert_eq!(config["log_level"].as_str(), Some("debug"));
    assert_eq!(
        config["catalog_dir"].as_str(),
        Some(fixture.data.to_str().unwrap())
    );
    assert!(backup_contains(&fixture.config, original.as_bytes()));
}

#[test]
fn frontend_role_installs_only_the_frontend_binaries() {
    let fixture = Fixture::new();

    let result = fixture.run("frontend", &[]);

    assert_success(&result);
    assert_eq!(fixture.installed(), ["narsor", "narsor-cli"]);
}

#[test]
fn server_role_requires_oidc_and_proxy_settings_before_writing() {
    let fixture = Fixture::new();

    let result = fixture.run("server", &[]);

    assert!(!result.status.success());
    let output = output(&result);
    for setting in [
        "server-bind",
        "oidc-issuer",
        "oidc-audience",
        "trusted-proxy",
        "backend-id",
    ] {
        assert!(output.contains(setting), "missing {setting} in: {output}");
    }
    assert!(!fixture.config.exists());
    assert!(!fixture.bin.exists());
}

#[test]
fn server_role_writes_network_configuration_and_a_workstation_backend() {
    let fixture = Fixture::new();

    let result = fixture.run(
        "server",
        &[
            "--server-bind",
            "0.0.0.0:7400",
            "--oidc-issuer",
            "https://identity.example",
            "--oidc-audience",
            "narsor",
            "--trusted-proxy",
            "10.0.0.0/8",
            "--backend-id",
            "vm-worker",
        ],
    );

    assert_success(&result);
    let config = read_yaml(&fixture.config);
    assert_eq!(config["server_mode"].as_str(), Some("network"));
    assert_eq!(config["server_bind"].as_str(), Some("0.0.0.0:7400"));
    assert_eq!(config["tls_termination"].as_str(), Some("proxy"));
    assert_eq!(
        config["trusted_proxies"]
            .as_sequence()
            .and_then(|items| items.first())
            .and_then(serde_yaml::Value::as_str),
        Some("10.0.0.0/8")
    );
    let backend = fixture.data.join("backends/vm-worker.yaml");
    let backend = read_yaml(&backend);
    assert_eq!(backend["kind"].as_str(), Some("local"));
    assert_eq!(backend["trust_scope"].as_str(), Some("workstation"));
    let message = output(&result);
    assert!(message.contains(
        "NARSOR_SERVER='https://<corporate-server>' narsor-cli controller enroll-code vm-worker"
    ));
    assert!(message.contains(&format!(
        "Start the network server separately: narsor-server --config '{}'",
        fixture.config.display()
    )));
    assert!(message.contains("--accept-workstation-risk"));
    assert!(message.contains("without a container"));
    assert_eq!(
        fixture.installed(),
        ["narsor-controller", "narsor-runner", "narsor-server"]
    );
}

#[test]
fn server_role_can_also_install_frontend_binaries() {
    let fixture = Fixture::new();

    let result = fixture.run(
        "server",
        &[
            "--with-frontend",
            "--server-bind",
            "0.0.0.0:7400",
            "--oidc-issuer",
            "https://identity.example",
            "--oidc-audience",
            "narsor",
            "--trusted-proxy",
            "10.0.0.0/8",
            "--backend-id",
            "vm-worker",
        ],
    );

    assert_success(&result);
    assert_eq!(
        fixture.installed(),
        [
            "narsor",
            "narsor-cli",
            "narsor-controller",
            "narsor-runner",
            "narsor-server",
        ]
    );
}

#[test]
fn server_role_uses_effective_environment_settings() {
    let fixture = Fixture::new();
    let catalog_dir = fixture.root.join("effective-catalog");

    let result = fixture.run_with_overrides(
        "server",
        &[
            "--oidc-issuer",
            "https://identity.example",
            "--oidc-audience",
            "narsor",
            "--trusted-proxy",
            "10.0.0.0/8",
            "--backend-id",
            "vm-worker",
        ],
        Some(&catalog_dir),
        Some("10.20.0.5:7400"),
    );

    assert_success(&result);
    assert!(catalog_dir.join("backends/vm-worker.yaml").is_file());
    let config = read_yaml(&fixture.config);
    assert!(config.get("catalog_dir").is_none());
    assert!(config.get("server_bind").is_none());
}

#[test]
fn server_role_rejects_loopback_binds_before_writing() {
    let fixture = Fixture::new();

    let result = fixture.run(
        "server",
        &[
            "--server-bind",
            "127.0.0.1:7400",
            "--oidc-issuer",
            "https://identity.example",
            "--oidc-audience",
            "narsor",
            "--trusted-proxy",
            "10.0.0.0/8",
            "--backend-id",
            "vm-worker",
        ],
    );

    assert!(!result.status.success());
    assert!(output(&result).contains("cannot bind to loopback"));
    assert!(!fixture.config.exists());
    assert!(!fixture.bin.exists());
}

#[test]
fn explicit_bashrc_update_is_idempotent() {
    let fixture = Fixture::new();

    assert_success(&fixture.run("local", &["--update-bashrc"]));
    assert_success(&fixture.run("local", &["--update-bashrc"]));

    let bashrc = fs::read_to_string(fixture.root.join(".bashrc")).unwrap();
    assert_eq!(
        bashrc
            .lines()
            .filter(|line| line.contains("export PATH="))
            .count(),
        1
    );
}

#[test]
fn bashrc_is_unchanged_without_explicit_consent() {
    let fixture = Fixture::new();

    assert_success(&fixture.run("local", &[]));

    assert!(!fixture.root.join(".bashrc").exists());
}

#[test]
fn release_bootstrap_verifies_download_and_cleans_temporary_files() {
    let fixture = Fixture::new();
    let tools = fixture.root.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let archive = fixture.root.join(LINUX_ARCHIVE);
    let manifest = fixture.source.join("manifest.json");
    fs::write(
        &manifest,
        "{\"target\": \"x86_64-unknown-linux-gnu\", \"binaries\": []}\n",
    )
    .unwrap();
    fs::write(
        fixture.source.join("narsor-cli"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NARSOR_CLI_CAPTURE\"\n",
    )
    .unwrap();
    let mut tar = Command::new("tar");
    tar.args(["-C", fixture.source.to_str().unwrap(), "-czf"])
        .arg(&archive)
        .arg("manifest.json")
        .args(RELEASE_BINARIES);
    let result = tar.output().expect("create fake release archive");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let digest = Command::new("sha256sum")
        .arg(&archive)
        .output()
        .expect("hash fake release archive");
    assert!(digest.status.success());
    let checksum = fixture.root.join(format!("{LINUX_ARCHIVE}.sha256"));
    let hash = String::from_utf8_lossy(&digest.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::write(&checksum, format!("{hash}  {LINUX_ARCHIVE}\n")).unwrap();
    let curl = tools.join("curl");
    fs::write(
        &curl,
        r#"#!/bin/sh
set -eu
url=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) shift; output="$1" ;;
        https://*) url="$1" ;;
    esac
    shift
done
printf '%s\n' "$(dirname "$output")" > "$NARSOR_INSTALL_CAPTURE"
case "$url" in
    *.sha256) cp "$NARSOR_TEST_CHECKSUM" "$output" ;;
    *) cp "$NARSOR_TEST_ARCHIVE" "$output" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();

    let captured_dir = fixture.root.join("bootstrap-work-dir");
    let cli_capture = fixture.root.join("cli-arguments");
    let mut path_entries = vec![tools];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries).unwrap();
    let mut command = Command::new("bash");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/install.sh"))
        .args([
            "--role",
            "frontend",
            "--non-interactive",
            "--bin-dir",
            "/tmp/narsor-bin",
            "--data-dir",
            "/tmp/narsor-data",
        ])
        .env("PATH", path)
        .env("NARSOR_INSTALL_CAPTURE", &captured_dir)
        .env("NARSOR_CLI_CAPTURE", &cli_capture)
        .env("NARSOR_TEST_ARCHIVE", &archive)
        .env("NARSOR_TEST_CHECKSUM", &checksum);
    let result = command.output().expect("run release bootstrap");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let work_dir = fs::read_to_string(captured_dir).unwrap();
    let work_dir = PathBuf::from(work_dir.trim());
    assert!(!work_dir.exists(), "bootstrap temp dir was not cleaned");
    let arguments = fs::read_to_string(cli_capture).unwrap();
    assert!(arguments.contains("install\n"));
    assert!(arguments.contains("--source-dir\n"));
    assert!(arguments.contains("--role\nfrontend\n"));
}

#[test]
fn invalid_configuration_is_left_unchanged() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.config.parent().expect("config parent")).unwrap();
    let original = b"unknown_setting: keep\n";
    fs::write(&fixture.config, original).unwrap();

    let result = fixture.run("local", &[]);

    assert!(!result.status.success());
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert!(!fixture.bin.exists());
}

fn assert_success(result: &Output) {
    assert!(result.status.success(), "{}", output(result));
}

fn output(result: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    )
}

fn read_yaml(path: &Path) -> serde_yaml::Value {
    serde_yaml::from_slice(&fs::read(path).expect("read YAML file")).expect("parse YAML")
}

fn backup_contains(path: &Path, contents: &[u8]) -> bool {
    let parent = path.parent().expect("config parent");
    let prefix = format!("{}.bak.", path.file_name().unwrap().to_string_lossy());
    fs::read_dir(parent)
        .expect("read config directory")
        .filter_map(Result::ok)
        .any(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && fs::read(entry.path()).is_ok_and(|backup| backup == contents)
        })
}
