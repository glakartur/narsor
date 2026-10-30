//! End-to-end tests of `dist/install.sh` against a fake release download.
//!
//! The archive holds stub binaries, except that `narsor-cli` forwards to the
//! real one built by Cargo, so the script and `narsor-cli init` run together.

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

struct Release {
    root: PathBuf,
    archive: PathBuf,
    checksum: PathBuf,
    tools: PathBuf,
}

impl Release {
    fn new(narsor_cli: &str) -> Self {
        let root = std::env::temp_dir().join(format!("narsor-script-{}", uuid::Uuid::new_v4()));
        let source = root.join("release");
        let tools = root.join("tools");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&tools).unwrap();
        for name in RELEASE_BINARIES {
            write_executable(&source.join(name), &format!("#!/bin/sh\n# {name}\n"));
        }
        write_executable(&source.join("narsor-cli"), narsor_cli);
        fs::write(
            source.join("manifest.json"),
            "{\"target\": \"x86_64-unknown-linux-gnu\", \"binaries\": []}\n",
        )
        .unwrap();

        let archive = root.join(LINUX_ARCHIVE);
        let packed = Command::new("tar")
            .args(["-C", source.to_str().unwrap(), "-czf"])
            .arg(&archive)
            .arg("manifest.json")
            .args(RELEASE_BINARIES)
            .output()
            .expect("create fake release archive");
        assert!(packed.status.success(), "{}", output(&packed));
        let digest = Command::new("sha256sum").arg(&archive).output().unwrap();
        let hash = String::from_utf8_lossy(&digest.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let checksum = root.join(format!("{LINUX_ARCHIVE}.sha256"));
        fs::write(&checksum, format!("{hash}  {LINUX_ARCHIVE}\n")).unwrap();

        write_executable(
            &tools.join("curl"),
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
printf '%s\n' "$url" >> "$NARSOR_URL_LOG"
printf '%s\n' "$(dirname "$output")" > "$NARSOR_WORK_DIR_LOG"
case "$url" in
    *.sha256) cp "$NARSOR_TEST_CHECKSUM" "$output" ;;
    *) cp "$NARSOR_TEST_ARCHIVE" "$output" ;;
esac
"#,
        );
        Self {
            root,
            archive,
            checksum,
            tools,
        }
    }

    /// A `narsor-cli` that runs the real binary built by Cargo.
    fn with_real_cli() -> Self {
        Self::new(&format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_narsor-cli")
        ))
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn config(&self) -> PathBuf {
        self.root.join("config/config.yaml")
    }

    fn bashrc(&self) -> PathBuf {
        self.root.join(".bashrc")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut path = vec![self.tools.clone()];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new("bash");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/install.sh"))
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("NARSOR_URL_LOG", self.root.join("urls"))
            .env("NARSOR_WORK_DIR_LOG", self.root.join("work-dir"))
            .env("NARSOR_TEST_ARCHIVE", &self.archive)
            .env("NARSOR_TEST_CHECKSUM", &self.checksum);
        command
    }

    /// Installs non-interactively into this release's own directories.
    fn install(&self, role: &str, extra: &[&str]) -> Output {
        let (bin, data, config) = (self.bin(), self.data(), self.config());
        let mut args = vec![
            "--role",
            role,
            "--non-interactive",
            "--bin-dir",
            bin.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "--config-file",
            config.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.command(&args).output().expect("run install.sh")
    }

    fn uninstall(&self, extra: &[&str]) -> Output {
        let (bin, data, config) = (self.bin(), self.data(), self.config());
        let mut args = vec![
            "--uninstall",
            "--non-interactive",
            "--bin-dir",
            bin.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "--config-file",
            config.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.command(&args).output().expect("run install.sh")
    }

    fn installed(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.bin()) else {
            return Vec::new();
        };
        let mut names = entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect::<Vec<String>>();
        names.sort();
        names
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const SERVER_ARGS: &[&str] = &[
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
];

#[test]
fn bootstrap_verifies_download_and_cleans_temporary_files() {
    let release = Release::new("#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NARSOR_CLI_CAPTURE\"\n");
    let capture = release.root.join("cli-arguments");

    let result = release
        .command(&[
            "--role",
            "frontend",
            "--non-interactive",
            "--bin-dir",
            release.bin().to_str().unwrap(),
            "--data-dir",
            "/tmp/narsor-data",
        ])
        .env("NARSOR_CLI_CAPTURE", &capture)
        .output()
        .unwrap();

    assert_success(&result);
    let work_dir = fs::read_to_string(release.root.join("work-dir")).unwrap();
    assert!(
        !Path::new(work_dir.trim()).exists(),
        "bootstrap temp dir was not cleaned"
    );
    let arguments = fs::read_to_string(capture).unwrap();
    assert!(arguments.starts_with("init\n"), "{arguments}");
    assert!(arguments.contains("--role\nfrontend\n"));
    assert!(arguments.contains("--non-interactive\n"));
    assert!(!arguments.contains("--bin-dir"), "{arguments}");
}

#[test]
fn installer_downloads_the_requested_version() {
    let release = Release::with_real_cli();

    assert_success(&release.install("frontend", &[]));
    let urls = fs::read_to_string(release.root.join("urls")).unwrap();
    assert!(urls.contains("/releases/latest/download/"), "{urls}");

    fs::remove_file(release.root.join("urls")).unwrap();
    let result = release
        .command(&[
            "v0.2.1",
            "--role",
            "frontend",
            "--non-interactive",
            "--bin-dir",
            release.bin().to_str().unwrap(),
            "--data-dir",
            release.data().to_str().unwrap(),
            "--config-file",
            release.config().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_success(&result);
    let urls = fs::read_to_string(release.root.join("urls")).unwrap();
    assert!(urls.contains("/releases/download/v0.2.1/"), "{urls}");
}

#[test]
fn roles_install_their_binaries_and_write_configuration() {
    let release = Release::with_real_cli();

    assert_success(&release.install("frontend", &[]));
    assert_eq!(release.installed(), ["narsor", "narsor-cli"]);
    assert!(release.config().is_file());

    let release = Release::with_real_cli();
    assert_success(&release.install("local", &[]));
    let mut all = RELEASE_BINARIES.to_vec();
    all.sort_unstable();
    assert_eq!(release.installed(), all);

    let release = Release::with_real_cli();
    assert_success(&release.install("server", SERVER_ARGS));
    assert_eq!(
        release.installed(),
        ["narsor-controller", "narsor-runner", "narsor-server"]
    );
}

#[test]
fn server_role_can_also_install_frontend_binaries() {
    let release = Release::with_real_cli();
    let mut extra = vec!["--with-frontend"];
    extra.extend_from_slice(SERVER_ARGS);

    assert_success(&release.install("server", &extra));

    let mut all = RELEASE_BINARIES.to_vec();
    all.sort_unstable();
    assert_eq!(release.installed(), all);
}

#[test]
fn invalid_configuration_leaves_the_binary_directory_untouched() {
    let release = Release::with_real_cli();
    fs::create_dir_all(release.config().parent().unwrap()).unwrap();
    fs::write(release.config(), "unknown_setting: keep\n").unwrap();

    let result = release.install("local", &[]);

    assert!(!result.status.success());
    assert!(!release.bin().exists());
    assert_eq!(
        fs::read(release.config()).unwrap(),
        b"unknown_setting: keep\n"
    );
}

#[test]
fn non_interactive_install_needs_role_and_bin_dir() {
    let release = Release::with_real_cli();

    let result = release
        .command(&["--non-interactive", "--bin-dir", "/tmp/unused"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(output(&result).contains("requires --role"));

    let result = release
        .command(&["--non-interactive", "--role", "local"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(output(&result).contains("requires --bin-dir"));
}

#[test]
fn explicit_bashrc_update_is_idempotent_and_needs_consent() {
    let release = Release::with_real_cli();

    assert_success(&release.install("local", &[]));
    assert!(!release.bashrc().exists());

    assert_success(&release.install("local", &["--update-bashrc"]));
    assert_success(&release.install("local", &["--update-bashrc"]));

    let bashrc = fs::read_to_string(release.bashrc()).unwrap();
    assert_eq!(
        bashrc
            .lines()
            .filter(|line| line.contains("export PATH="))
            .count(),
        1,
        "{bashrc}"
    );
}

#[test]
fn uninstall_removes_binaries_and_path_entry_but_keeps_configuration() {
    let release = Release::with_real_cli();
    fs::write(release.bashrc(), "alias ll='ls -l'").unwrap();
    assert_success(&release.install("local", &["--update-bashrc"]));
    // Not ours: a binary with another name in the same directory stays.
    fs::write(release.bin().join("other-tool"), "#!/bin/sh\n").unwrap();
    // `init` only names the data directory; the first run creates it.
    fs::create_dir_all(release.data()).unwrap();

    let result = release.uninstall(&[]);

    assert_success(&result);
    assert_eq!(release.installed(), ["other-tool"]);
    assert!(release.config().is_file());
    assert!(release.data().is_dir());
    assert_eq!(
        fs::read_to_string(release.bashrc()).unwrap(),
        "alias ll='ls -l'\n"
    );
    assert!(output(&result).contains("--purge"));
}

#[test]
fn uninstall_does_not_download_anything() {
    let release = Release::with_real_cli();
    assert_success(&release.install("frontend", &[]));
    fs::remove_file(release.root.join("urls")).unwrap();

    assert_success(&release.uninstall(&[]));

    assert!(!release.root.join("urls").exists());
}

#[test]
fn purge_also_removes_configuration_backups_and_data() {
    let release = Release::with_real_cli();
    fs::create_dir_all(release.config().parent().unwrap()).unwrap();
    fs::write(release.config(), "log_level: debug\n").unwrap();
    assert_success(&release.install("local", &[]));
    assert!(fs::read_dir(release.config().parent().unwrap())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".bak.")));
    fs::create_dir_all(release.data()).unwrap();
    fs::write(release.data().join("state.db"), "x").unwrap();

    let result = release.uninstall(&["--purge"]);

    assert_success(&result);
    assert!(release.installed().is_empty());
    assert!(!release.config().exists());
    assert!(!release.data().exists());
}

#[test]
fn purge_leaves_directories_outside_the_data_directory() {
    let release = Release::with_real_cli();
    let outside = release.root.join("shared-catalog");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(release.config().parent().unwrap()).unwrap();
    fs::write(
        release.config(),
        format!("catalog_dir: {}\n", outside.display()),
    )
    .unwrap();
    assert_success(&release.install("local", &[]));

    let result = release.uninstall(&["--purge"]);

    assert_success(&result);
    assert!(outside.is_dir());
    assert!(output(&result).contains("Left in place"));
}

#[test]
fn purge_refuses_a_data_directory_that_contains_home() {
    let release = Release::with_real_cli();
    assert_success(&release.install("frontend", &[]));
    let root = release.root.to_str().unwrap().to_string();
    let bin = release.bin();
    let config = release.config();

    let result = release
        .command(&[
            "--uninstall",
            "--purge",
            "--non-interactive",
            "--bin-dir",
            bin.to_str().unwrap(),
            "--data-dir",
            &root,
            "--config-file",
            config.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(output(&result).contains("contains the home directory"));
    assert!(release.root.exists());
    assert!(release.config().exists());
    // Binaries are untouched because the purge failed first.
    assert_eq!(release.installed(), ["narsor", "narsor-cli"]);
}

#[test]
fn purge_without_binaries_explains_the_problem() {
    let release = Release::with_real_cli();

    let result = release.uninstall(&["--purge"]);

    assert!(!result.status.success());
    assert!(output(&result).contains("needed for --purge"));
}

#[test]
fn purge_requires_uninstall() {
    let release = Release::with_real_cli();

    let result = release.command(&["--purge"]).output().unwrap();

    assert!(!result.status.success());
    assert!(output(&result).contains("--purge works only with --uninstall"));
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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
