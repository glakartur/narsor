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

    fn command(&self, code: Option<&str>, extra: &[&str]) -> Command {
        let mut command = Command::new(engine_support::workspace_binary("narsor-controller"));
        command
            .arg("local")
            .arg("--control-plane")
            .arg(self.harness.base_url())
            .arg("--credential-file")
            .arg(&self.credential)
            .arg("--storage")
            .arg(&self.storage)
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
    let pid = std::fs::read_to_string(setup.credential.with_file_name("controller.lock")).unwrap();
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
