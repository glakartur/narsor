//! `narsor-cli secret set` and the local controller's token store (KTD10):
//! the client stores a secret where the controller the daemon starts reads
//! it.

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser as _;
use narsor::cli::commands::secret_set_args;
use narsor_controller::local::container::secrets::SecretStore;
use narsor_controller::local::paths::ControllerStatePaths;
use narsor_controller::local::{run_local_helper, LocalCommand};
use narsor_engine::config::cascade::ConfigCascade;
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

/// `narsor-controller`'s `local` helper commands, as the binary parses
/// them.
#[derive(Debug, clap::Parser)]
struct Controller {
    #[command(subcommand)]
    backend: Backend,
}

#[derive(Debug, clap::Subcommand)]
enum Backend {
    Local {
        #[command(subcommand)]
        command: LocalCommand,
    },
}

fn state_dir_of(config: &Path, scope: Scope) -> PathBuf {
    let cascade = ConfigCascade::with_dirs(Some(config.to_path_buf()), None, None);
    let resolved = resolve_with(&Flags::default(), &Environment::new(), &cascade, scope);
    assert!(resolved.is_ok(), "{:?}", resolved.errors());
    resolved.state_dir().to_path_buf()
}

#[test]
fn a_secret_set_through_the_cli_lands_in_the_state_of_the_controller_the_daemon_starts() {
    let temp = TempDir::new();
    let config = temp.config_file();

    // narsor-cli and the daemon resolve the state root from the same file;
    // narsor-cli hands it to the controller, which finds its store below it.
    let cli_args = secret_set_args(&state_dir_of(&config, Scope::CLI), "gitlab-token");
    let daemon_state_root = state_dir_of(&config, Scope::SERVER);
    let state_root_arg = cli_args
        .iter()
        .position(|arg| arg == "--state-root")
        .map(|flag| PathBuf::from(&cli_args[flag + 1]));
    assert_eq!(state_root_arg.as_deref(), Some(daemon_state_root.as_path()));

    let parsed = Controller::try_parse_from(
        std::iter::once(OsString::from("narsor-controller")).chain(cli_args),
    )
    .expect("the controller accepts the arguments");
    let Backend::Local { command } = parsed.backend;
    run_local_helper(command, &mut "glpat-secret\n".as_bytes()).expect("stored");

    let read = SecretStore::of(&ControllerStatePaths::new(&daemon_state_root))
        .read("gitlab-token")
        .unwrap();
    assert_eq!(read.as_deref(), Some("glpat-secret"));
    assert!(daemon_state_root
        .join("narsor-controller-local/secrets/gitlab-token")
        .is_file());
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

    let read = SecretStore::of(&ControllerStatePaths::new(temp.0.join("state")))
        .read("gitlab-token")
        .unwrap();
    assert_eq!(read.as_deref(), Some("glpat-secret"));
}
