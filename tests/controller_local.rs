//! `narsor-controller local` as a process (U2): enrollment and reuse of its
//! identity, the credential lock, the parent watch and a stop that releases
//! the lease (R5, R8, R10, R12; AE7).
//!
//! The served control plane runs on the test's runtime, so every test uses
//! the multi-threaded one: waiting for a process blocks a thread.
//!
//! Each test starts real `narsor-controller` processes against a served
//! control plane in this process, so they are `#[ignore]`d like the daemon
//! tests; run them with `--ignored`.

#[path = "engine_support/mod.rs"]
mod engine_support;
mod environment_support;
mod remote_support;
/// The stub `narsor-runner` of the contract tests, at the module path its
/// program selects.
mod backend_contract_support {
    pub mod stub_runner;
}

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use narsor_engine::controller_protocol::CreateEnrollmentRequest;
use narsor_engine::domain::BackendId;
use narsor_engine::orchestration::{BackendLease, Clock, ControlPlane};
use narsor_engine::persistence::paths::StatePaths;
use remote_support::RemoteHarness;

/// A controller process, killed when the test lets go of it.
struct ControllerProcess(Child);

impl ControllerProcess {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn wait_within(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().expect("wait for the controller") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stderr(mut self) -> String {
        use std::io::Read as _;
        let mut text = String::new();
        if let Some(mut stderr) = self.0.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }
}

impl Drop for ControllerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Where one controller of a test runs from and what it keeps.
struct Place {
    program: PathBuf,
    control_plane: String,
    credential: PathBuf,
    storage: PathBuf,
}

struct Setup {
    harness: RemoteHarness,
    credential: PathBuf,
    storage: PathBuf,
}

impl Setup {
    async fn new() -> Self {
        let harness = RemoteHarness::start().await;
        let credential = harness.root.path().join("controller/credential");
        let storage = harness.root.path().join("controller-work");
        Self {
            harness,
            credential,
            storage,
        }
    }

    async fn code(&self) -> String {
        self.harness
            .owner
            .create_enrollment(
                "local",
                &CreateEnrollmentRequest {
                    accept_workstation_risk: true,
                    ..CreateEnrollmentRequest::default()
                },
            )
            .await
            .expect("an enrollment code")
            .enrollment_code
    }

    /// The test's own controller: the built binary, over loopback.
    fn place(&self) -> Place {
        Place {
            program: engine_support::workspace_binary("narsor-controller"),
            control_plane: self.harness.base_url(),
            credential: self.credential.clone(),
            storage: self.storage.clone(),
        }
    }

    /// The control plane's address as one beyond this machine: an
    /// IPv4-mapped address is not loopback to a controller.
    fn remote_url(&self) -> String {
        format!(
            "http://[::ffff:127.0.0.1]:{}",
            self.harness.handle.addr().port()
        )
    }

    fn command(&self, code: Option<&str>, extra: &[&str]) -> Command {
        self.command_at(&self.place(), code, extra)
    }

    fn command_at(&self, place: &Place, code: Option<&str>, extra: &[&str]) -> Command {
        let mut command = Command::new(&place.program);
        command
            .arg("local")
            .arg("--control-plane")
            .arg(&place.control_plane)
            .arg("--credential-file")
            .arg(&place.credential)
            .arg("--storage")
            .arg(&place.storage)
            .args(extra)
            .env_remove("NARSOR_ENROLLMENT_CODE")
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(code) = code {
            command.env("NARSOR_ENROLLMENT_CODE", code);
        }
        command
    }

    fn start(&self, code: Option<&str>, extra: &[&str]) -> ControllerProcess {
        ControllerProcess(
            self.command(code, extra)
                .spawn()
                .expect("start narsor-controller"),
        )
    }

    fn lease(&self) -> Option<BackendLease> {
        ControlPlane::new(StatePaths::new(self.harness.root.path()))
            .lease(&BackendId::parse("local").unwrap())
            .expect("read the lease")
    }

    fn active(&self, lease: &Option<BackendLease>) -> bool {
        lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > self.harness.clock.now())
    }

    async fn wait_for_lease(
        &self,
        what: &str,
        accept: impl Fn(&BackendLease) -> bool,
    ) -> BackendLease {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let lease = self.lease();
            if self.active(&lease) && accept(lease.as_ref().unwrap()) {
                return lease.unwrap();
            }
            assert!(Instant::now() < deadline, "the lease never {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn send(pid: u32, signal: i32) {
    // SAFETY: signals a process this test started.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn it_enrolls_holds_the_lease_and_releases_it_on_sigterm() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    let mut controller = setup.start(Some(&code), &[]);
    let held = setup.wait_for_lease("became active", |_| true).await;
    assert!(setup.credential.is_file(), "the credential was written");
    assert_eq!(mode(&setup.credential), 0o600);

    send(controller.id(), libc::SIGTERM);
    let status = controller
        .wait_within(Duration::from_secs(10))
        .expect("the controller stops on SIGTERM");
    assert!(status.success(), "{status:?}");
    let released = setup.lease().expect("a lease record");
    assert_eq!(released.epoch, held.epoch);
    assert!(
        released.expires_at <= setup.harness.clock.now(),
        "the lease is released at once, not left to expire: {}",
        controller.stderr()
    );
}

/// Covers AE7.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn a_second_start_reuses_the_identity_without_a_code() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    let mut first = setup.start(Some(&code), &[]);
    let held = setup.wait_for_lease("became active", |_| true).await;
    send(first.id(), libc::SIGTERM);
    first.wait_within(Duration::from_secs(10)).expect("stopped");
    let enrolled = std::fs::read_to_string(&setup.credential).unwrap();

    let _second = setup.start(None, &[]);
    let again = setup
        .wait_for_lease("was taken again", |lease| lease.epoch > held.epoch)
        .await;
    assert_eq!(again.controller_id, held.controller_id);
    assert_eq!(
        std::fs::read_to_string(&setup.credential).unwrap(),
        enrolled
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn without_a_credential_or_a_code_it_is_a_usage_error() {
    let setup = Setup::new().await;
    let mut controller = setup.start(None, &[]);
    let status = controller
        .wait_within(Duration::from_secs(10))
        .expect("the controller exits");
    assert_eq!(status.code(), Some(2));
    let text = controller.stderr();
    assert!(text.contains("NARSOR_ENROLLMENT_CODE"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn a_second_controller_on_the_same_credential_waits_and_gives_up_naming_the_holder() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    let first = setup.start(Some(&code), &[]);
    setup.wait_for_lease("became active", |_| true).await;

    let mut second = setup.start(None, &["--lock-timeout-secs", "1"]);
    let status = second
        .wait_within(Duration::from_secs(10))
        .expect("the second controller gives up");
    assert_eq!(status.code(), Some(1));
    let text = second.stderr();
    assert!(text.contains(&first.id().to_string()), "{text}");
}

/// The lock is the credential's, not its directory's: a controller of
/// another backend whose credential lies beside it does not wait.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn controllers_whose_credentials_share_a_directory_do_not_wait_for_each_other() {
    use narsor_engine::environment::{BackendKind, BackendRecord, EnvironmentCatalog, TrustScope};

    let setup = Setup::new().await;
    let code = setup.code().await;
    let _first = setup.start(Some(&code), &[]);
    setup.wait_for_lease("became active", |_| true).await;

    EnvironmentCatalog::new(setup.harness.root.path())
        .save_backend(&BackendRecord::new(
            "laptop",
            BackendKind::Local,
            TrustScope::Workstation,
            BackendRecord::builtin_local().limits,
        ))
        .expect("register the laptop");
    let code = setup
        .harness
        .owner
        .create_enrollment(
            "laptop",
            &CreateEnrollmentRequest {
                accept_workstation_risk: true,
                ..CreateEnrollmentRequest::default()
            },
        )
        .await
        .expect("a code")
        .enrollment_code;
    let beside = Place {
        credential: setup.credential.with_file_name("laptop-credential"),
        storage: setup.harness.root.path().join("laptop-work"),
        ..setup.place()
    };
    let mut second = ControllerProcess(
        setup
            .command_at(&beside, Some(&code), &["--lock-timeout-secs", "1"])
            .spawn()
            .expect("start the second controller"),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let lease = ControlPlane::new(StatePaths::new(setup.harness.root.path()))
            .lease(&BackendId::parse("laptop").unwrap())
            .expect("read the lease");
        if lease.is_some() {
            break;
        }
        assert!(
            second.wait_within(Duration::ZERO).is_none(),
            "the second controller gave up: {}",
            second.stderr()
        );
        assert!(Instant::now() < deadline, "the laptop's lease never came");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn a_waiting_controller_takes_over_once_the_holder_is_killed() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    let first = setup.start(Some(&code), &[]);
    let held = setup.wait_for_lease("became active", |_| true).await;

    let mut second = setup.start(None, &["--lock-timeout-secs", "20"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(second.wait_within(Duration::ZERO).is_none(), "it waits");
    send(first.id(), libc::SIGKILL);
    let again = setup
        .wait_for_lease("was taken by the second controller", |lease| {
            lease.epoch > held.epoch
        })
        .await;
    assert_eq!(again.controller_id, held.controller_id);
    let pid = std::fs::read_to_string(setup.credential.with_file_name("credential.lock")).unwrap();
    assert_eq!(pid.trim(), second.id().to_string());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn it_stops_and_releases_the_lease_when_its_parent_is_gone() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    // A shell stands in for the daemon: the controller is its child and
    // names it as its parent.
    let command = setup.command(Some(&code), &[]);
    let program = command.get_program().to_owned();
    let args: Vec<_> = command.get_args().map(|arg| arg.to_owned()).collect();
    let mut shell = Command::new("sh");
    shell
        .arg("-c")
        .arg(r#"program="$1"; shift; "$program" "$@" --parent-pid $$ & echo $!; wait"#)
        .arg("sh")
        .arg(program)
        .args(args)
        .env("NARSOR_ENROLLMENT_CODE", &code)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut parent = ControllerProcess(shell.spawn().expect("start the shell"));
    let pid: u32 = {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(parent.0.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        line.trim().parse().expect("the controller's pid")
    };
    let held = setup.wait_for_lease("became active", |_| true).await;

    send(parent.id(), libc::SIGKILL);
    parent
        .wait_within(Duration::from_secs(5))
        .expect("the shell died");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Path::new(&format!("/proc/{pid}")).exists() && !is_zombie(pid) {
        assert!(
            Instant::now() < deadline,
            "the controller outlived its parent"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let released = setup.lease().expect("a lease record");
    assert_eq!(released.epoch, held.epoch);
    assert!(released.expires_at <= setup.harness.clock.now());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn sigterm_with_the_control_plane_gone_does_not_hang() {
    let setup = Setup::new().await;
    let code = setup.code().await;
    let mut controller = setup.start(Some(&code), &[]);
    setup.wait_for_lease("became active", |_| true).await;
    let Setup { harness, .. } = setup;
    harness.handle.stop_accepting();
    drop(harness);

    let started = Instant::now();
    send(controller.id(), libc::SIGTERM);
    controller
        .wait_within(Duration::from_secs(8))
        .expect("the controller stops within its release bound");
    assert!(started.elapsed() < Duration::from_secs(8));
}

fn is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// Transport to a remote control plane (U6: R16, AE13)
// ---------------------------------------------------------------------------

mod transport {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use narsor_controller::local::{check_transport, ControlPlanePlace};
    use narsor_engine::controller_protocol::{ClientOptions, ControllerClient, RunnerClient};

    /// What a release build accepts: no plain HTTP beyond loopback.
    const RELEASE: bool = false;

    /// Covers AE13 (`https://`).
    #[test]
    fn plain_http_beyond_loopback_is_refused_in_a_release_build() {
        for flag in ["--control-plane", "--runner-control-plane"] {
            let error =
                check_transport(flag, "http://10.0.0.5:8443", RELEASE).expect_err("refused");
            assert_eq!(error.code, 2);
            assert!(error.message.contains("https://"), "{}", error.message);
            assert!(error.message.contains(flag), "{}", error.message);
        }
        assert_eq!(
            check_transport("--control-plane", "https://narsor.example", RELEASE).unwrap(),
            ControlPlanePlace::Remote
        );
    }

    #[test]
    fn loopback_is_an_address_or_exactly_localhost() {
        for url in [
            "http://127.0.0.1:7400",
            "http://127.1.2.3:7400",
            "http://[::1]:7400",
            "http://localhost:7400",
            "https://localhost",
        ] {
            assert_eq!(
                check_transport("--control-plane", url, RELEASE).expect(url),
                ControlPlanePlace::Loopback,
                "{url}"
            );
        }
        for url in [
            "http://localhost.example.com",
            "http://127.0.0.1.nip.io",
            "http://0.0.0.0:7400",
        ] {
            assert!(
                check_transport("--control-plane", url, RELEASE).is_err(),
                "{url} is not loopback"
            );
        }
    }

    #[test]
    fn other_schemes_and_user_information_are_refused() {
        for url in [
            "ftp://narsor.example",
            "https://me:pw@narsor.example",
            "narsor",
        ] {
            assert!(
                check_transport("--control-plane", url, true).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn no_flag_turns_the_certificate_check_off() {
        let output = std::process::Command::new(super::engine_support::workspace_binary(
            "narsor-controller",
        ))
        .args(["local", "--help"])
        .output()
        .unwrap();
        let help = String::from_utf8_lossy(&output.stdout).to_lowercase();
        for word in [
            "insecure",
            "invalid",
            "no-verify",
            "skip-tls",
            "certificate",
        ] {
            assert!(!help.contains(word), "{word} in:\n{help}");
        }
    }

    /// A control plane that redirects elsewhere is not followed: a signed
    /// request, an enrollment code or a runner credential never reaches
    /// another host.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_redirect_to_another_host_is_not_followed() {
        let elsewhere_hits = Arc::new(AtomicUsize::new(0));
        let elsewhere = {
            let hits = Arc::clone(&elsewhere_hits);
            let app = axum::Router::new().fallback(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { "{}" }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            addr
        };
        let redirecting = {
            let location = format!("http://{elsewhere}/controller/v1/enroll");
            let app = axum::Router::new().fallback(move || {
                let location = location.clone();
                async move {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [(axum::http::header::LOCATION, location)],
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });
            addr
        };

        let refused = ControllerClient::enroll_with_options(
            format!("http://{redirecting}"),
            "the-code",
            &ClientOptions {
                refuse_redirects: true,
                ..ClientOptions::default()
            },
        )
        .await;
        assert!(refused.is_err());
        assert_eq!(elsewhere_hits.load(Ordering::SeqCst), 0, "not followed");
        // Nor does a runner carry its credential there.
        let refused = RunnerClient::new(format!("http://{redirecting}"), "runner-secret")
            .renew()
            .await;
        assert!(refused.is_err());
        assert_eq!(elsewhere_hits.load(Ordering::SeqCst), 0, "not followed");

        // Without the option, the same request would have gone there.
        let _ = ControllerClient::enroll(format!("http://{redirecting}"), "the-code").await;
        assert!(
            elsewhere_hits.load(Ordering::SeqCst) > 0,
            "the redirect is real"
        );
    }
}

// ---------------------------------------------------------------------------
// A control plane beyond this machine: HOME and the configuration (U7)
// ---------------------------------------------------------------------------

mod untrusted_server {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    /// A control plane beyond loopback that never answers: its name does
    /// not resolve, so a controller that got past its checks fails to
    /// enroll at once.
    const REMOTE: &str = "https://narsor.invalid";

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-untrusted-cli-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(dir.join("config")).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Writes the user's configuration file, found by the usual search.
        fn user_config(&self, text: &str) {
            std::fs::create_dir_all(self.0.join("config/narsor")).unwrap();
            std::fs::write(self.0.join("config/narsor/config.yaml"), text).unwrap();
        }

        fn file(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn local(dir: &Dir, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command =
            Command::new(super::engine_support::workspace_binary("narsor-controller"));
        for (name, _) in std::env::vars() {
            if name.starts_with("NARSOR_") {
                command.env_remove(name);
            }
        }
        command
            .arg("local")
            .arg("--control-plane")
            .arg(REMOTE)
            .arg("--storage")
            .arg(dir.path().join("storage"))
            .arg("--credential-file")
            .arg(dir.path().join("state/credential"))
            .args(args)
            .env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_STATE_HOME", dir.path().join("state-home"))
            .env("NARSOR_ENROLLMENT_CODE", "code")
            .envs(envs.iter().copied())
            .output()
            .expect("run narsor-controller local")
    }

    fn stderr(output: &Output) -> String {
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Past every check: it went on to enroll, with this HOME mode.
    fn started_with(output: &Output, mode: &str) {
        let text = stderr(output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert!(text.contains("enrollment failed"), "{text}");
        assert!(text.contains(&format!("HOME mode '{mode}'")), "{text}");
    }

    fn refused_host_home(output: &Output) {
        let text = stderr(output);
        assert_eq!(output.status.code(), Some(2), "{text}");
        assert!(text.contains("--accept-host-home"), "{text}");
        assert!(text.contains("SSH keys"), "{text}");
        assert!(!text.contains("enrollment failed"), "{text}");
    }

    /// Covers AE13 (`--home host`).
    #[test]
    fn host_home_needs_the_users_explicit_acceptance() {
        let dir = Dir::new();
        refused_host_home(&local(&dir, &["--home", "host"], &[]));

        let accepted = local(&dir, &["--home", "host", "--accept-host-home"], &[]);
        started_with(&accepted, "host");
        assert!(stderr(&accepted).contains("WARNING: --accept-host-home"));
    }

    #[test]
    fn without_any_setting_tasks_get_a_private_home_and_the_user_is_warned() {
        let dir = Dir::new();
        let output = local(&dir, &[], &[]);
        started_with(&output, "private");
        let text = stderr(&output);
        assert!(text.contains(REMOTE), "{text}");
        assert!(text.contains("WARNING"), "{text}");
        assert!(text.contains("without a container"), "{text}");
    }

    /// Covers AE14.
    #[test]
    fn host_home_from_the_configuration_needs_the_same_acceptance() {
        let dir = Dir::new();
        let config = dir.file("config.yaml", "local_home: host\n");
        let config = config.to_str().unwrap();
        refused_host_home(&local(&dir, &["--config", config], &[]));
        started_with(
            &local(&dir, &["--config", config, "--home", "private"], &[]),
            "private",
        );
    }

    #[test]
    fn a_flag_wins_over_the_environment_which_wins_over_the_file() {
        let dir = Dir::new();
        let private_file = dir.file("private.yaml", "local_home: private\n");
        let host_file = dir.file("host.yaml", "local_home: host\n");
        let host_env = [("NARSOR_LOCAL_HOME", "host")];
        let private_env = [("NARSOR_LOCAL_HOME", "private")];

        // The flag over the environment.
        started_with(&local(&dir, &["--home", "private"], &host_env), "private");
        // The environment over the file.
        refused_host_home(&local(
            &dir,
            &["--config", private_file.to_str().unwrap()],
            &host_env,
        ));
        started_with(
            &local(
                &dir,
                &["--config", host_file.to_str().unwrap()],
                &private_env,
            ),
            "private",
        );
        // The file alone.
        refused_host_home(&local(
            &dir,
            &["--config", host_file.to_str().unwrap()],
            &[],
        ));
    }

    #[test]
    fn no_config_skips_the_configuration_file() {
        let dir = Dir::new();
        dir.user_config("local_home: host\n");
        refused_host_home(&local(&dir, &[], &[]));
        started_with(&local(&dir, &["--no-config"], &[]), "private");
    }

    /// A harness directory is the user's own login: like `--home host`, it
    /// needs `--accept-host-home`.
    #[test]
    fn a_harness_directory_flag_needs_the_users_explicit_acceptance() {
        let dir = Dir::new();
        for flag in [
            "--codex-home",
            "--claude-config-dir",
            "--claude-profile-source",
        ] {
            let output = local(&dir, &[flag, "/home/me/.logins"], &[]);
            let text = stderr(&output);
            assert_eq!(output.status.code(), Some(2), "{text}");
            assert!(text.contains(flag), "{text}");
            assert!(text.contains("--accept-host-home"), "{text}");
            assert!(!text.contains("enrollment failed"), "{text}");
        }
        let accepted = local(
            &dir,
            &["--codex-home", "/home/me/.codex", "--accept-host-home"],
            &[],
        );
        started_with(&accepted, "private");
        assert!(
            stderr(&accepted).contains("CODEX_HOME=/home/me/.codex"),
            "{}",
            stderr(&accepted)
        );
    }

    /// The configuration file's harness directories are the daemon's: a
    /// control plane beyond this machine does not get them unasked.
    #[test]
    fn harness_directories_from_the_configuration_are_left_out() {
        let dir = Dir::new();
        dir.user_config(
            "local_codex_home: /home/me/.codex\nlocal_claude_config_dir: /home/me/.claude\n",
        );
        let output = local(&dir, &[], &[]);
        started_with(&output, "private");
        assert!(
            !stderr(&output).contains("/home/me/."),
            "{}",
            stderr(&output)
        );

        let accepted = local(&dir, &["--accept-host-home"], &[]);
        started_with(&accepted, "private");
        let text = stderr(&accepted);
        assert!(text.contains("CODEX_HOME=/home/me/.codex"), "{text}");
        assert!(
            text.contains("CLAUDE_CONFIG_DIR=/home/me/.claude"),
            "{text}"
        );
    }

    #[test]
    fn a_file_without_local_home_leaves_a_private_home() {
        let dir = Dir::new();
        dir.user_config("log_level: warn\n");
        started_with(&local(&dir, &[], &[]), "private");
    }
}

/// A controller serving a control plane it does not trust — reached under
/// an IPv4-mapped address, which is not loopback to it — says which backend
/// it serves and holds that backend's lease.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts narsor-controller processes; run with --ignored"]
async fn a_controller_of_a_remote_control_plane_names_its_backend() {
    use narsor_engine::environment::{BackendKind, BackendRecord, EnvironmentCatalog, TrustScope};

    let setup = Setup::new().await;
    let record = BackendRecord::new(
        "laptop",
        BackendKind::Local,
        TrustScope::Workstation,
        BackendRecord::builtin_local().limits,
    );
    EnvironmentCatalog::new(setup.harness.root.path())
        .save_backend(&record)
        .expect("register the laptop");
    let code = setup
        .harness
        .owner
        .create_enrollment(
            "laptop",
            &CreateEnrollmentRequest {
                accept_workstation_risk: true,
                ..CreateEnrollmentRequest::default()
            },
        )
        .await
        .expect("a code")
        .enrollment_code;
    let port = setup.harness.handle.addr().port();
    let command = setup.command(Some(&code), &[]);
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut remote = Command::new(command.get_program());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--control-plane" {
            let _ = args.next();
            remote
                .arg(arg)
                .arg(format!("http://[::ffff:127.0.0.1]:{port}"));
        } else {
            remote.arg(arg);
        }
    }
    let state = setup.harness.root.path().join("home");
    std::fs::create_dir_all(state.join("config")).unwrap();
    remote
        .arg("--no-config")
        .env("NARSOR_ENROLLMENT_CODE", &code)
        .env("HOME", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    drop(command);
    let mut controller = ControllerProcess(remote.spawn().expect("start narsor-controller"));

    let deadline = Instant::now() + Duration::from_secs(15);
    let lease = loop {
        let lease = ControlPlane::new(StatePaths::new(setup.harness.root.path()))
            .lease(&BackendId::parse("laptop").unwrap())
            .expect("read the lease");
        if lease.is_some() {
            break lease.unwrap();
        }
        assert!(Instant::now() < deadline, "the laptop's lease never came");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(lease.backend_id.as_str(), "laptop");
    send(controller.id(), libc::SIGTERM);
    controller
        .wait_within(Duration::from_secs(10))
        .expect("stopped");
    let text = controller.stderr();
    assert!(text.contains("serving backend 'laptop'"), "{text}");
    assert!(text.contains("HOME mode 'private'"), "{text}");
}

// ---------------------------------------------------------------------------
// Runners of a controller process (U2: R5, R12; U7: R17)
// ---------------------------------------------------------------------------

/// What a controller's runners do: each runner here is a script beside the
/// controller that writes down its environment and arguments and then runs
/// the stub runner, which speaks the runner protocol without running a
/// workflow step.
mod runners {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use narsor_engine::controller_protocol::{
        CreateEnrollmentRequest, SubmitOperatorCommandRequest, WorkspaceRequest,
    };
    use narsor_engine::domain::{ExecutionStatus, ResourceProfile, TaskId};
    use narsor_engine::environment::{
        BackendKind, BackendLimits, BackendRecord, EnvironmentCatalog, TrustScope,
    };
    use narsor_engine::orchestration::{ControlPlane, OperatorCommand, RunnerState, TaskRecord};
    use narsor_engine::persistence::paths::StatePaths;

    use super::backend_contract_support::stub_runner;
    use super::{remote_support, ControllerProcess, Place, Setup};

    /// A directory holding a copy of the controller and, when `runner` is
    /// set, a `narsor-runner` beside it that records how it was started in
    /// `runner-env` and `runner-args` before it runs the stub. The
    /// controller starts the runner beside its own executable.
    fn controller_dir(dir: &Path, runner: bool) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let program = dir.join("narsor-controller");
        let built = super::engine_support::workspace_binary("narsor-controller");
        if std::fs::hard_link(&built, &program).is_err() {
            std::fs::copy(&built, &program).unwrap();
        }
        if runner {
            let stub = stub_runner::install(&dir.join("stub"));
            let script = dir.join("narsor-runner");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nenv > '{dir}/runner-env'\nprintf '%s\\n' \"$@\" > \
                     '{dir}/runner-args'\nexec '{stub}' \"$@\"\n",
                    dir = dir.display(),
                    stub = stub.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        program
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn record(setup: &Setup, task: &str) -> TaskRecord {
        ControlPlane::new(StatePaths::new(setup.harness.root.path()))
            .task(&TaskId::parse(task).unwrap())
            .expect("the task")
    }

    fn detail(record: &TaskRecord) -> String {
        record
            .observed
            .as_ref()
            .and_then(|observed| observed.summary.detail.clone())
            .unwrap_or_default()
    }

    async fn wait_for(
        setup: &Setup,
        task: &str,
        what: &str,
        accept: impl Fn(&TaskRecord) -> bool,
    ) -> TaskRecord {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let record = record(setup, task);
            if accept(&record) {
                return record;
            }
            assert!(
                Instant::now() < deadline,
                "{task}: never {what}; last observed: {:?}",
                record.observed
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn create(setup: &Setup, task: &str, backend: &str, repository: Option<&Path>) {
        let mut request = remote_support::create_request(task, Some(backend));
        request.launch.workspace = repository.map(|repository| WorkspaceRequest {
            repository: Some(repository.display().to_string()),
            commit: None,
        });
        setup
            .harness
            .owner
            .create_control_task(&request)
            .await
            .unwrap_or_else(|error| panic!("create {task}: {error}"));
    }

    /// A Git repository with one commit, inside the directories the control
    /// plane may take a local workspace from.
    fn repository(setup: &Setup) -> PathBuf {
        let repo = setup.harness.root.path().join("users-repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "first",
            ][..],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        repo
    }

    /// A workstation backend `laptop` for two tasks at once, and a code
    /// for it.
    async fn laptop(setup: &Setup) -> String {
        EnvironmentCatalog::new(setup.harness.root.path())
            .save_backend(&BackendRecord::new(
                "laptop",
                BackendKind::Local,
                TrustScope::Workstation,
                BackendLimits::new(2, ResourceProfile::new(1000, 2048), None),
            ))
            .expect("register the laptop");
        setup
            .harness
            .owner
            .create_enrollment(
                "laptop",
                &CreateEnrollmentRequest {
                    accept_workstation_risk: true,
                    ..CreateEnrollmentRequest::default()
                },
            )
            .await
            .expect("a code")
            .enrollment_code
    }

    /// Covers AE7 for a whole task: started, run to its end, on a runner
    /// that inherits none of the controller's own `NARSOR_*` settings and
    /// gets the HOME mode the controller was given.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts narsor-controller processes; run with --ignored"]
    async fn a_task_runs_to_its_end_on_a_runner_without_the_controllers_settings() {
        let setup = Setup::new().await;
        let dir = setup.harness.root.path().join("trusted");
        let place = Place {
            program: controller_dir(&dir, true),
            ..setup.place()
        };
        let code = setup.code().await;
        let mut command = setup.command_at(&place, Some(&code), &["--home", "host"]);
        command.env("NARSOR_PLANTED_SETTING", "controller-only");
        let _controller = ControllerProcess(command.spawn().expect("start narsor-controller"));

        create(&setup, "task-1", "local", None).await;
        wait_for(&setup, "task-1", "started", |record| {
            record.runner_state == RunnerState::Running
        })
        .await;
        setup
            .harness
            .owner
            .control_operator_command(
                "task-1",
                &SubmitOperatorCommandRequest {
                    command: OperatorCommand::Transition {
                        target: "done".to_string(),
                        forced: false,
                        reason: None,
                    },
                    idempotency_key: "finish".to_string(),
                    expected_generation: None,
                },
            )
            .await
            .expect("move the task to its end");
        wait_for(&setup, "task-1", "completed", |record| {
            record.runner.as_ref().is_some_and(|runner| {
                runner.snapshot.execution_status == ExecutionStatus::Completed
            })
        })
        .await;

        let env = lines(&dir.join("runner-env"));
        let narsor: Vec<&String> = env
            .iter()
            .filter(|line| line.starts_with("NARSOR_"))
            .filter(|line| !line.starts_with("NARSOR_LOCAL_WORKLOAD_UID="))
            .collect();
        assert!(narsor.is_empty(), "{narsor:?}");
        let args = lines(&dir.join("runner-args"));
        assert!(
            args.windows(2).any(|pair| pair == ["--home", "host"]),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "--pinned-git-credential-only"),
            "a trusted daemon's runner clones as the user: {args:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts narsor-controller processes; run with --ignored"]
    async fn without_a_runner_beside_it_a_launch_is_refused_and_says_why() {
        let setup = Setup::new().await;
        let dir = setup.harness.root.path().join("alone");
        let place = Place {
            program: controller_dir(&dir, false),
            ..setup.place()
        };
        let code = setup.code().await;
        let _controller = ControllerProcess(
            setup
                .command_at(&place, Some(&code), &[])
                .spawn()
                .expect("start narsor-controller"),
        );
        create(&setup, "task-1", "local", None).await;
        let record = wait_for(&setup, "task-1", "was refused", |record| {
            detail(record).contains("refused the workload")
        })
        .await;
        let why = detail(&record);
        assert!(why.contains("cannot start"), "{why}");
        assert!(
            why.contains(&dir.join("narsor-runner").display().to_string()),
            "{why}"
        );
    }

    /// Variables of the user's own logins a runner of a server that is not
    /// trusted never inherits.
    const USER_LOGINS: [&str; 9] = [
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
        "GH_TOKEN",
        "GITLAB_TOKEN",
        "SSH_AUTH_SOCK",
        "GIT_CONFIG_COUNT",
        "XDG_CONFIG_HOME",
    ];

    /// Covers AE13 and AE14 end to end: a controller of a control plane
    /// beyond this machine refuses a workspace of the user's disk before
    /// anything of the task exists, and starts the runner of a task it
    /// accepts so that it — its clone and its harness — inherits none of the
    /// user's logins. The same workspace from a trusted daemon still starts.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "starts narsor-controller processes; run with --ignored"]
    async fn a_remote_control_plane_cannot_have_the_users_repository_cloned() {
        let setup = Setup::new().await;
        let repo = repository(&setup);
        let home = setup.harness.root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let remote_dir = setup.harness.root.path().join("remote");
        let remote = Place {
            program: controller_dir(&remote_dir, true),
            control_plane: setup.remote_url(),
            credential: remote_dir.join("credential"),
            storage: remote_dir.join("storage"),
        };
        let code = laptop(&setup).await;
        let mut command = setup.command_at(&remote, Some(&code), &["--no-config"]);
        command.env("HOME", &home);
        for name in USER_LOGINS {
            command.env(name, "planted-by-the-user");
        }
        let _remote = ControllerProcess(command.spawn().expect("start the remote controller"));

        create(&setup, "users-repo", "laptop", Some(&repo)).await;
        let refused = wait_for(&setup, "users-repo", "was refused", |record| {
            detail(record).contains("refused the workload")
        })
        .await;
        let why = detail(&refused);
        assert!(why.contains("https://"), "{why}");
        assert!(
            !remote_dir.join("runner-args").exists(),
            "no runner was started"
        );
        let storages = remote.storage.join("storage");
        assert!(
            std::fs::read_dir(&storages).map_or(true, |mut entries| entries.next().is_none()),
            "no task storage was made"
        );

        create(&setup, "no-workspace", "laptop", None).await;
        wait_for(&setup, "no-workspace", "got a runner", |record| {
            record.runner.is_some()
        })
        .await;
        let args = lines(&remote_dir.join("runner-args"));
        assert!(
            args.windows(2).any(|pair| pair == ["--home", "private"]),
            "{args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "--pinned-git-credential-only"),
            "{args:?}"
        );
        let env = lines(&remote_dir.join("runner-env"));
        let inherited: Vec<&String> = env
            .iter()
            .filter(|line| {
                USER_LOGINS
                    .iter()
                    .any(|name| line.starts_with(&format!("{name}=")))
            })
            .collect();
        assert!(inherited.is_empty(), "{inherited:?}");

        // The trusted daemon's controller takes the same workspace.
        let trusted_dir = setup.harness.root.path().join("trusted");
        let trusted = Place {
            program: controller_dir(&trusted_dir, true),
            ..setup.place()
        };
        let code = setup.code().await;
        let mut command = setup.command_at(&trusted, Some(&code), &[]);
        for name in ["CODEX_HOME", "GH_TOKEN"] {
            command.env(name, "planted-by-the-user");
        }
        let _trusted = ControllerProcess(command.spawn().expect("start the trusted controller"));
        create(&setup, "trusted-repo", "local", Some(&repo)).await;
        let started = wait_for(&setup, "trusted-repo", "got a runner", |record| {
            record.runner.is_some() || detail(record).contains("refused")
        })
        .await;
        assert!(started.runner.is_some(), "{:?}", started.observed);
        // Its runner keeps the user's logins.
        let env = lines(&trusted_dir.join("runner-env"));
        for name in ["CODEX_HOME", "GH_TOKEN"] {
            let line = format!("{name}=planted-by-the-user");
            assert!(env.contains(&line), "{name}: {env:?}");
        }
    }
}
