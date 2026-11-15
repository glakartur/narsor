//! The process group a harness runs in.
//!
//! The harness is started as the leader of a group of its own, so everything
//! it starts — a polling loop in `Bash`, a subagent's shell — can be ended
//! together with it. `kill_on_drop` and `Child::kill` only reach the leader:
//! a background task left behind keeps the stdout pipe open and the step
//! waiting on a process that has nothing more to say.
//!
//! Signals are sent by number, not through the `Child`: the executor waits on
//! the child while it signals. A group lives as long as one member does, so
//! signalling it after the leader was reaped still reaches what the leader
//! left behind; `ESRCH` then means there is nothing to end. The number of a
//! group that no member is left in could be reused by an unrelated process;
//! the window is the time between the last member's exit and the signal, and
//! is not worth guarding against.

/// Sends `signal` to every process in the group `pgid`. `true` when the group
/// had a member to receive it.
pub(super) fn signal(pgid: i32, signal: i32) -> bool {
    // SAFETY: `killpg` takes two integers and touches no memory of ours.
    unsafe { libc::killpg(pgid, signal) == 0 }
}

pub(super) const TERMINATE: i32 = libc::SIGTERM;
pub(super) const KILL: i32 = libc::SIGKILL;

/// Kills the group when dropped while armed — when the future running the
/// harness is cancelled, which `kill_on_drop` alone would leave to the
/// leader. The executor disarms it once the group has been dealt with.
pub(super) struct GroupGuard(Option<i32>);

impl GroupGuard {
    /// A guard over the group led by `leader`, the harness's process id.
    pub(super) fn new(leader: Option<u32>) -> Self {
        Self(leader.and_then(|pid| i32::try_from(pid).ok()))
    }

    /// The group's number, when the harness reported its process id.
    pub(super) fn pgid(&self) -> Option<i32> {
        self.0
    }

    /// Stops the guard from killing the group on drop.
    pub(super) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            signal(pgid, KILL);
        }
    }
}
