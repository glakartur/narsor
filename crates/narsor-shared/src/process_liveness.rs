//! Whether a recorded process still runs: its pid together with its start
//! time, so a reused pid is never taken for the recorded process.
//!
//! The local controller records both for every runner it starts; the
//! server's one-time layout migration reads the same records to refuse a
//! move while a runner lives (KTD10).

use std::fs;

/// Field 22 of `/proc/{pid}/stat`: when the process started, in clock ticks
/// since boot. `None` for a process that is gone or a zombie.
pub fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; fields resume after
    // the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    if state == "Z" {
        return None;
    }
    // `rest` starts at field 3 (state); field 22 is 19 further on.
    fields.nth(18)?.parse().ok()
}

/// Whether `pid` still names the process that started at `start_ticks`
/// and has not terminated (a zombie counts as terminated). Without a
/// recorded start time any live process of that pid counts.
pub fn process_alive(pid: u32, start_ticks: Option<u64>) -> bool {
    match (process_start_ticks(pid), start_ticks) {
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_process_counts_only_with_its_own_start_time() {
        let pid = std::process::id();
        let ticks = process_start_ticks(pid).expect("this process has a start time");
        assert!(process_alive(pid, Some(ticks)));
        assert!(process_alive(pid, None));
        assert!(!process_alive(pid, Some(ticks + 1)), "a reused pid");
    }
}
