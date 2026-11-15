//! The daemon's Linux capabilities: only the observer worker gets them.
//!
//! A server service that enables the worker with a script user is given
//! [`WORKER_CAPABILITIES`] as ambient capabilities (for systemd:
//! `AmbientCapabilities=`). Ambient capabilities survive `execve`, so
//! without care every child of the daemon — the local controller, its
//! runners, the agents — would hold them too.
//!
//! * [`confine_daemon`] runs first in `main`, while the daemon has one
//!   thread (capabilities belong to a thread, and new threads copy them):
//!   it clears the daemon's ambient set and removes from its effective set
//!   what came from the ambient set. The permitted set stays, and the
//!   worker's capabilities are put into the inheritable set, so that the
//!   worker can still get them.
//! * [`clear_ambient`] runs in every child before `exec`
//!   ([`crate::child_process::prepare`]): the child starts with no
//!   capability.
//! * [`raise_worker_ambient`] runs in the observer worker's child only,
//!   after [`clear_ambient`]: the worker starts with exactly
//!   [`WORKER_CAPABILITIES`], if the daemon holds them.

use std::io;

/// `CAP_CHOWN`: give the script's run directory to the script user.
const CAP_CHOWN: u32 = 0;
/// `CAP_KILL`: stop the script user's processes.
const CAP_KILL: u32 = 5;
/// `CAP_SETGID`: switch to the script user's group.
const CAP_SETGID: u32 = 6;
/// `CAP_SETUID`: switch to the script user.
const CAP_SETUID: u32 = 7;

/// The capabilities the observer worker needs to run scripts as another
/// user.
pub const WORKER_CAPABILITIES: [u32; 4] = [CAP_SETUID, CAP_SETGID, CAP_CHOWN, CAP_KILL];

/// `_LINUX_CAPABILITY_VERSION_3`: two 32-bit words per set.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `SECBIT_NOROOT`: user id 0 gets no capabilities from `execve`.
const SECBIT_NOROOT: libc::c_int = 1;

/// `struct __user_cap_header_struct`.
#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

/// `struct __user_cap_data_struct`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The worker's capabilities as a mask of the first 32-bit word (all of
/// them are below 32).
fn worker_mask() -> u32 {
    WORKER_CAPABILITIES
        .iter()
        .fold(0, |mask, capability| mask | 1 << capability)
}

/// Clears the calling thread's ambient set, removes from its effective set
/// what the ambient set held, and adds to its inheritable set the worker's
/// capabilities it holds as permitted. Call it before the daemon starts a
/// thread. A process with root's privileges keeps its effective set: its
/// children get every capability back from `execve` anyway.
pub fn confine_daemon() -> io::Result<()> {
    let mut ambient = [0_u32; 2];
    for capability in 0..64_u32 {
        // SAFETY: `prctl` with integer arguments only.
        let held = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_IS_SET,
                libc::c_ulong::from(capability),
                0,
                0,
            )
        };
        if held == 1 {
            ambient[(capability / 32) as usize] |= 1 << (capability % 32);
        }
    }
    clear_ambient()?;
    let mut sets = get()?;
    if !has_root_privileges() {
        for (set, ambient) in sets.iter_mut().zip(ambient) {
            set.effective &= !ambient;
        }
    }
    sets[0].inheritable |= sets[0].permitted & worker_mask();
    set(&sets)
}

/// Clears the calling thread's ambient set. Safe between `fork` and
/// `exec`: one `prctl`, no allocation. A kernel without ambient
/// capabilities (`EINVAL`) has nothing to clear.
pub fn clear_ambient() -> io::Result<()> {
    // SAFETY: `prctl` with integer arguments only.
    let result = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINVAL) {
            return Err(error);
        }
    }
    Ok(())
}

/// Raises [`WORKER_CAPABILITIES`] in the calling thread's ambient set.
/// Safe between `fork` and `exec`. A capability the daemon does not hold
/// is skipped: the worker then reports what it cannot do.
pub fn raise_worker_ambient() -> io::Result<()> {
    for capability in WORKER_CAPABILITIES {
        // SAFETY: `prctl` with integer arguments only. `EPERM` (not held)
        // and `EINVAL` (no ambient capabilities) leave the set as it is.
        unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE,
                libc::c_ulong::from(capability),
                0,
                0,
            );
        }
    }
    Ok(())
}

/// User id 0 without `SECBIT_NOROOT`.
fn has_root_privileges() -> bool {
    // SAFETY: `geteuid` and `prctl` with integer arguments only.
    unsafe {
        libc::geteuid() == 0
            && libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) & SECBIT_NOROOT == 0
    }
}

fn get() -> io::Result<[CapabilityData; 2]> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut sets = [CapabilityData::default(); 2];
    // SAFETY: `capget` writes two data structs into `sets`, which holds two.
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, sets.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sets)
}

fn set(sets: &[CapabilityData; 2]) -> io::Result<()> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    // SAFETY: `capset` reads a version-3 header and two data structs, both
    // valid for the duration of the call.
    if unsafe { libc::syscall(libc::SYS_capset, &mut header, sets.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::child_process;
    use crate::observer_worker::{ObserverWorkerSettings, ScriptIsolation};

    /// A capability set line of `/proc/<pid>/status`, as a number.
    fn status_set(status: &str, name: &str) -> u64 {
        let line = status
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .unwrap_or_else(|| panic!("no {name} in {status}"));
        u64::from_str_radix(line.trim(), 16).unwrap()
    }

    /// What a child sees of its own capabilities: `/proc/self/status`.
    fn child_status(mut command: Command) -> String {
        let output = command.output().unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// A child prepared like the local controller, running `sh`, which
    /// prints its own status to stdout.
    fn local_controller_child() -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat /proc/self/status"]);
        child_process::prepare(&mut command, None);
        command.stdout(std::process::Stdio::piped());
        command
    }

    /// The observer worker's child, with a stand-in program that prints
    /// its own status to stdout.
    fn worker_child(dir: &std::path::Path, isolation: ScriptIsolation) -> Command {
        use std::os::unix::fs::PermissionsExt as _;
        let program = dir.join("print-status");
        std::fs::write(&program, "#!/bin/sh\ncat /proc/self/status\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let settings = ObserverWorkerSettings {
            program,
            server: "http://127.0.0.1:1".into(),
            secrets_dir: None,
            isolation,
            log_directive: None,
            supervision: narsor_controller_runtime::Supervision {
                restart_after: std::time::Duration::from_millis(50),
                stop_timeout: std::time::Duration::from_secs(2),
            },
        };
        let mut command = settings.command("key");
        command.stdout(std::process::Stdio::piped());
        command
    }

    #[test]
    fn the_worker_mask_holds_exactly_the_four_capabilities() {
        assert_eq!(worker_mask(), 0b1110_0001);
    }

    #[test]
    fn a_daemon_without_capabilities_confines_itself_and_its_children_start() {
        // No capability to move: a plain user. The calls must still work.
        std::thread::spawn(|| {
            confine_daemon().unwrap();
            let status = child_status(local_controller_child());
            assert_eq!(status_set(&status, "CapAmb"), 0, "{status}");
        })
        .join()
        .unwrap();
    }

    /// Run by [`children_of_a_daemon_with_ambient_capabilities_hold_none`]
    /// as a process that holds the worker's capabilities as ambient ones.
    #[test]
    #[ignore = "run by children_of_a_daemon_with_ambient_capabilities_hold_none"]
    fn confined_daemon_with_ambient_capabilities() {
        let own = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        assert_eq!(
            status_set(&own, "CapAmb"),
            u64::from(worker_mask()),
            "this test needs the worker's ambient capabilities: {own}"
        );
        // While the daemon still holds them: `prepare` alone keeps them
        // from the child.
        let child = child_status(local_controller_child());
        assert_eq!(status_set(&child, "CapAmb"), 0, "{child}");
        assert_eq!(status_set(&child, "CapEff"), 0, "{child}");
        assert_eq!(status_set(&child, "CapPrm"), 0, "{child}");

        confine_daemon().unwrap();
        let own = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        assert_eq!(status_set(&own, "CapAmb"), 0, "{own}");
        assert_eq!(status_set(&own, "CapEff"), 0, "{own}");
        let mask = u64::from(worker_mask());
        assert_eq!(status_set(&own, "CapPrm") & mask, mask, "{own}");
        assert_eq!(status_set(&own, "CapInh") & mask, mask, "{own}");

        let child = child_status(local_controller_child());
        assert_eq!(status_set(&child, "CapAmb"), 0, "{child}");
        assert_eq!(status_set(&child, "CapEff"), 0, "{child}");
        assert_eq!(status_set(&child, "CapPrm"), 0, "{child}");

        let dir = std::env::temp_dir().join(format!("narsor-caps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let same_user = child_status(worker_child(&dir, ScriptIsolation::SameUser));
        assert_eq!(status_set(&same_user, "CapAmb"), 0, "{same_user}");
        assert_eq!(status_set(&same_user, "CapEff"), 0, "{same_user}");

        let worker = child_status(worker_child(&dir, ScriptIsolation::User("nobody".into())));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(status_set(&worker, "CapAmb"), mask, "{worker}");
        assert_eq!(status_set(&worker, "CapEff"), mask, "{worker}");
    }

    /// Runs the test above in a user namespace, as user id 0 without
    /// root's privileges (`SECBIT_NOROOT`) and with the worker's
    /// capabilities as ambient ones — what a systemd service with
    /// `AmbientCapabilities=` gives a non-root daemon. Skipped where
    /// `unshare -r` or `setpriv` is not available.
    #[test]
    fn children_of_a_daemon_with_ambient_capabilities_hold_none() {
        let caps = "+setuid,+setgid,+chown,+kill";
        let available = Command::new("unshare")
            .args(["-r", "setpriv", "--ambient-caps", "-all", "true"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !available {
            eprintln!("skipped: `unshare -r` with `setpriv` is not available");
            return;
        }
        let output = Command::new("unshare")
            .arg("-r")
            .arg("setpriv")
            .args(["--securebits", "+noroot,+noroot_locked,+no_setuid_fixup"])
            .args(["--inh-caps", caps, "--ambient-caps", caps])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capabilities::tests::confined_daemon_with_ambient_capabilities",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
