//! A stub `narsor-runner` for the adapters that start runner *processes*
//! (the local adapter, the mock cluster's kubelet).
//!
//! The adapters under contract test start a program and manage it as a
//! runner: its process group, its exit code, its kill. What the control
//! plane and the controllers see of a runner is the protocol, so the stub
//! speaks it with the fake platform's simulated runner
//! ([`narsor_engine::backends::fake::run_simulated_runner`]): it runs no
//! workflow step, reports `Running` until an owner's `Transition` moves the
//! work to a terminal state, pauses at once at a safe point and leaves when
//! told. Its event cursor and workflow state live in `stub-runner/` on the
//! task storage it is given, so a resumed stub continues there; a read-only
//! stub (`--read-only`) writes nothing.
//!
//! # How the process starts
//!
//! The program is a shell script that runs the test binary itself, with
//! only the ignored test [`stub_runner_process`] selected and the runner's
//! arguments in an environment variable (the test harness accepts no other
//! arguments). That test does nothing when the variable is unset, so a run
//! with `--ignored` passes it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use narsor_engine::backends::fake::{run_simulated_reader, run_simulated_runner, SimulatedStorage};
use narsor_engine::controller_protocol::RunnerClient;

/// The variable that carries the runner's arguments, one per line.
const ARGS_ENV: &str = "NARSOR_STUB_RUNNER_ARGS";

/// The test-harness name of [`stub_runner_process`].
const ENTRY: &str = "backend_contract_support::stub_runner::stub_runner_process";

/// Writes the stub runner program into `dir` and returns its path.
///
/// The program is run once before it is returned: a child forked by another
/// test while the script was open for writing makes its first `exec` fail
/// with "Text file busy", and a program that ran once cannot fail so again.
pub fn install(dir: &Path) -> PathBuf {
    let exe = std::env::current_exe().expect("the test executable");
    fs::create_dir_all(dir).expect("the stub runner's directory");
    let program = dir.join("narsor-runner");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\n{ARGS_ENV}=\"$(printf '%s\\n' \"$@\")\"\nexport {ARGS_ENV}\n\
             exec '{}' --ignored --exact --nocapture --test-threads=1 {ENTRY}\n",
            exe.display()
        ),
    )
    .expect("write the stub runner");
    fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("make the stub runner executable");
    for _ in 0..50 {
        match Command::new(&program).arg("--probe").output() {
            Ok(output) => {
                assert!(output.status.success(), "the stub runner: {output:?}");
                return program;
            }
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("the stub runner: {error}"),
        }
    }
    panic!("the stub runner stayed busy");
}

/// The stub runner's process entry (see the module docs).
#[test]
#[ignore = "the stub runner's process entry; the contract harness starts it"]
fn stub_runner_process() {
    let Ok(args) = std::env::var(ARGS_ENV) else {
        return;
    };
    let code = run(&args.lines().map(str::to_string).collect::<Vec<_>>());
    std::process::exit(code);
}

/// Runs the stub on the runner's command line; its exit code.
fn run(args: &[String]) -> i32 {
    if args.first().is_some_and(|arg| arg == "--probe") {
        return 0;
    }
    let value = |flag: &str| -> String {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|at| args.get(at + 1))
            .unwrap_or_else(|| panic!("the stub runner needs {flag}: {args:?}"))
            .clone()
    };
    let credential = fs::read_to_string(value("--credential-file")).expect("the credential");
    let client = RunnerClient::new(value("--control-plane"), credential.trim());
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    if args.iter().any(|arg| arg == "--read-only") {
        return runtime.block_on(run_simulated_reader(&client, || {}));
    }
    let launch_spec: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(value("--launch-spec")).expect("the launch spec"))
            .expect("a launch spec");
    let storage = StubStorage(PathBuf::from(value("--task-storage")).join("stub-runner"));
    runtime.block_on(run_simulated_runner(&client, &launch_spec, &storage, || {}))
}

/// The stub's work on the task storage: one file per value.
struct StubStorage(PathBuf);

impl StubStorage {
    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.0.join(name)).ok()
    }

    fn write(&self, name: &str, value: &str) {
        fs::create_dir_all(&self.0).expect("the stub runner's storage");
        fs::write(self.0.join(name), value).expect("write the stub runner's storage");
    }
}

impl SimulatedStorage for StubStorage {
    fn next_event(&self) -> u64 {
        let cursor = self
            .read("cursor")
            .and_then(|text| text.parse::<u64>().ok())
            .unwrap_or(0)
            + 1;
        self.write("cursor", &cursor.to_string());
        cursor
    }

    fn workflow_state(&self) -> Option<String> {
        self.read("state")
    }

    fn enter_state(&self, state: &str) {
        self.write("state", state);
    }
}
