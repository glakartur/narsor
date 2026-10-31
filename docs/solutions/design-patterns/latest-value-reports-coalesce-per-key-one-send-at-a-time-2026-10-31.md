---
title: "Latest-value reports to the control plane: coalesce per key, send one at a time"
date: 2026-10-31
category: design-patterns
module: narsor-runner/runner/usage
problem_type: design_pattern
component: background_job
severity: medium
applies_when:
  - "A component pushes fire-and-forget 'latest state' reports (usage limits, progress, heartbeats) to the control plane"
  - "Reports come from several sources (harnesses) that share one channel"
  - "The receiver keeps only the last report it got, so arrival order decides the stored state"
  - "A failed send must not be retried and must not touch the link backoff"
tags:
  - runner
  - usage-limits
  - harness
  - ordering
  - coalescing
  - tokio
  - watch-channel
  - fire-and-forget
---

# Latest-value reports to the control plane: coalesce per key, send one at a time

## Context

The runner forwards the subscription limits of each harness (`HarnessUsageReport`, R8) to `narsor-server`. The server keeps only the last report per harness in memory. The first version had two defects, found in review:

1. **Reordering.** Each change of a `tokio::sync::watch` channel started `tokio::spawn(client.report_usage(..))`. Two sends could run at the same time. A slow older send could arrive after a newer one, and the server then stored stale limits.
2. **Lost reports of other harnesses.** One `watch` slot held the latest report of *all* harnesses. A `codex` report replaced an unsent `claude-code` report, so that harness's state never reached the server.

A first fix (`3964c6d`) kept the `watch` channel and gated `changed()` on `usage_send.is_none()`. That fixed the order, but not defect 2, and it spread the state over the `Session` loop (`usage_send`, `usage_open`, `wait_for`).

## Guidance

Put the whole policy in one small component (`UsageForwarder` in `crates/narsor-runner/src/runner/usage.rs`):

- **Buffer:** a `VecDeque` with at most one unsent report per key (harness). A new report replaces only the unsent report of the same key; a new key goes to the back.
- **One send at a time:** a `sending: bool` flag under the same `Mutex`. The send task clears it and calls `Notify::notify_one()` when it ends.
- **`ready()`:** async, cancel-safe, changes nothing. It resolves when a report waits and no send runs. Use it as a `tokio::select!` branch.
- **`forward(sender)`:** takes the oldest report and spawns the send. Without a sender (no link yet) the report is dropped (R8). A failed send is logged at `debug` and dropped: no retry, the next report of the key replaces it, and the snapshot backoff is not touched.
- **Sender behind a trait** (`UsageSender`, production impl on `RunnerClient`), so tests use a double instead of a TCP/HTTP server.

```rust
// Session loop: the loop only drives the forwarder.
() = self.usage.ready() => {
    self.usage.forward(self.link.client());
}
```

```rust
impl UsageReporter for UsageForwarder {
    fn report(&self, report: HarnessUsageReport) {
        {
            let mut state = self.lock();
            match state.waiting.iter_mut().find(|w| w.harness == report.harness) {
                Some(waiting) => *waiting = report,
                None => state.waiting.push_back(report),
            }
        }
        self.changed.notify_one();
    }
}
```

`Notify::notify_one()` stores a permit when nobody waits, so a signal sent between the `can_send()` check and `notified().await` is not lost.

## Why This Matters

When the receiver keeps "the last report it got", concurrent sends turn network latency into wrong state. A single-slot "latest" channel is correct only for one source; with several sources it silently drops data. Both defects do not show in happy-path tests, because local sends finish fast and in order.

## When to Apply

- Any runner-to-server report where only the newest value counts, per key.
- When you see `tokio::spawn` for each change of a `watch` channel that feeds a remote last-write-wins store.
- Not for reports that must all arrive (events, transcript entries): those need a queue with retry, not coalescing.

## Examples

Before (`watch` + spawn per change):

```rust
changed = self.usage.changed() => {
    let report = self.usage.borrow_and_update().clone()?;
    usage::send(client, report); // tokio::spawn, no wait; sends can overlap
}
```

After: `UsageForwarder` as above. Tests that prove the contract (`usage.rs` unit tests and `session_tests.rs`):

- a new report replaces only the unsent report of its harness;
- without a client, the oldest report is dropped and the next one waits;
- while a send blocks in the test double, a newer report waits and goes out only after the first send ends (`a_newer_usage_report_waits_for_the_send_under_way_and_arrives_last`).

Related hardening from the same review (`f76d270`): the server answers a report with `resets_at` in the past with `400 invalid_usage_report` and removes that harness's state; the Claude Code parser clamps finite `utilization` to `0..=1` before it sends.

## Related

- `docs/plans/2026-10-31-001-feat-limity-abonamentu-harnessow-plan.md` (R6a, R8)
- `ARCHITECTURE_DECISIONS.md` D70 (usage state only in memory)
- `docs/solutions/logic-errors/tui-list-read-on-every-runner-report-2026-10-22.md` (other side of runner reports: TUI reads)
