//! `narsor-cli secret set` and the local controller's token store (KTD10):
//! the client stores a secret where the controller the daemon starts reads
//! it.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use narsor::cli::commands::secret_set_args;
use narsor_controller::local::container::secrets::SecretStore;
use narsor_engine::config::cascade::ConfigCascade;
use narsor_engine::config::local_controller;
use narsor_engine::config::resolve::{resolve_with, Environment, Flags};
use narsor_engine::config::Scope;

/// A temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "narsor-secret-store-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create the directory");
        Self(dir)
    }

    /// A configuration file that puts the daemon's state under this
    /// directory.
    fn config_file(&self) -> PathBuf {
        let path = self.0.join("config.yaml");
        let state = self.0.join("state");
        fs::write(&path, format!("server:\n  state: {}\n", state.display()))
            .expect("write the configuration");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The value of `--credential-file` in `args`.
fn credential_file_arg(args: &[std::ffi::OsString]) -> PathBuf {
    let flag = args
        .iter()
        .position(|arg| arg == "--credential-file")
        .expect("--credential-file is passed");
    PathBuf::from(&args[flag + 1])
}

fn state_dir_of(config: &Path, scope: Scope) -> PathBuf {
    let cascade = ConfigCascade::with_dirs(Some(config.to_path_buf()), None, None);
    let resolved = resolve_with(&Flags::default(), &Environment::new(), &cascade, scope);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    resolved.state_dir().to_path_buf()
}

#[test]
fn a_secret_set_through_the_cli_is_read_by_the_controller_the_daemon_starts() {
    let temp = TempDir::new();
    let config = temp.config_file();

    // narsor-cli and the daemon resolve the state directory from the same
    // file; the daemon hands its controller the credential beside it.
    let cli_args = secret_set_args(&state_dir_of(&config, Scope::CLI), "gitlab-token");
    let daemon_credential =
        local_controller::credential_file(&state_dir_of(&config, Scope::SERVER));
    assert_eq!(credential_file_arg(&cli_args), daemon_credential);
    assert_eq!(
        cli_args.last().map(|arg| arg.as_os_str()),
        Some("gitlab-token".as_ref())
    );

    SecretStore::beside_credential(&credential_file_arg(&cli_args))
        .set("gitlab-token", "glpat-secret")
        .unwrap();
    let read = SecretStore::beside_credential(&daemon_credential)
        .read("gitlab-token")
        .unwrap();
    assert_eq!(read.as_deref(), Some("glpat-secret"));
}

#[test]
#[ignore = "runs the built narsor-cli and narsor-controller binaries"]
fn the_built_cli_stores_stdin_where_the_controller_reads_it() {
    let temp = TempDir::new();
    let config = temp.config_file();
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_narsor-cli"));
    assert!(
        cli.with_file_name("narsor-controller").is_file(),
        "build the workspace binaries first"
    );

    let mut child = Command::new(cli)
        .args(["secret", "set", "gitlab-token", "--config"])
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run narsor-cli");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"glpat-secret\n")
        .expect("write the value");
    let output = child.wait_with_output().expect("wait for narsor-cli");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("glpat-secret"), "the value was printed");

    let credential = local_controller::credential_file(&temp.0.join("state"));
    let read = SecretStore::beside_credential(&credential)
        .read("gitlab-token")
        .unwrap();
    assert_eq!(read.as_deref(), Some("glpat-secret"));
}
