---
title: "Test network features through trait seams and test doubles, not TCP/HTTP servers"
date: 2026-10-31
category: conventions
module: tests (runner, server, cli, tui)
problem_type: convention
component: testing_framework
severity: medium
applies_when:
  - "A feature sends to or reads from the control plane (runner reports, TUI reads, CLI commands)"
  - "A test would start a TcpListener, a stub server, or Harness::start only to check one behaviour"
  - "A test must hold a request in flight to check order, concurrency, or a hang"
  - "A test checks an HTTP detail (body limit, status code) of one route"
tags:
  - testing
  - test-double
  - seam
  - trait
  - tokio
  - axum
  - tower-oneshot
  - manual-clock
  - no-tcp
---

# Test network features through trait seams and test doubles, not TCP/HTTP servers

## Context

The first tests of the harness usage limits (plan `docs/plans/2026-10-31-001-feat-limity-abonamentu-harnessow-plan.md`) started real servers: the runner session tests used a `UsageServer` on a `TcpListener`, the protocol tests called `Harness::start()`, and the TUI and CLI tests started a `Stub` server. These tests were slow, needed free ports, and could not hold one request in flight in a controlled way. One test (past `resets_at` over HTTP) also repeated a service test.

Commits `ca0c7d1` and `f55c6bf` moved all of them to seams with test doubles. No usage test now opens a socket.

## Guidance

### 1. One narrow trait per remote call, the real client implements it

Put the remote call behind a small trait that returns a boxed future. The production client implements it with a clone of itself; the code under test holds an `Arc<dyn Trait>`.

```rust
pub type UsageRead = Pin<Box<dyn Future<Output = Result<HarnessUsageResponse, ClientError>> + Send>>;

pub trait UsageSource: Send + Sync {
    fn read_usage(&self) -> UsageRead;
}

impl UsageSource for ApiClient {
    fn read_usage(&self) -> UsageRead {
        let client = self.clone();
        Box::pin(async move { client.control_usage().await })
    }
}
```

Seams from this feature:

| Area | Seam | How the test injects it |
|---|---|---|
| runner | `usage::UsageSender` (impl on `RunnerClient`) | `session.usage_sender = Some(Arc::new(double))` |
| TUI | `api::UsageSource` (impl on `ApiClient`) | `TaskApp::with_usage_source(Arc<dyn UsageSource>)` |
| CLI | `api::UsageSource` | `usage_output(&dyn UsageSource, json, now) -> Result<String>` |

The trait is narrow (one call). Do not mock the whole `ApiClient`.

### 2. Test doubles record, answer, fail, and can hold a call

A useful double has these parts (see `UsageControlPlane` in `crates/narsor-runner/src/runner/session_tests.rs` and `UsageDouble` in `tests/tui_background.rs`):

- **record:** `received: Mutex<Vec<_>>` or a `reads: AtomicUsize` counter;
- **answer / refuse:** a configurable answer, or `Err(ClientError::Unavailable(..))`;
- **hold:** a gate (`Notify` or `Semaphore`) that keeps the call in flight until the test releases it;
- **arrived signal:** `Notify` or `reached(n)` so the test waits for the call to start, with a timeout, not a sleep.

With a gate the test checks order and concurrency deterministically:

```rust
let first = forward_usage(session).await;               // send 1 starts
timeout(PATIENCE, plane.arrived.notified()).await?;     // it reached the double
reporter.report(usage_report(0.3));
assert!(session.usage.forward(sender).is_none());       // no 2nd send in flight
plane.release.notify_one();                             // let send 1 end
sent(first).await;
sent(forward_usage(session).await).await;
assert_eq!(plane.received(), vec![0.2, 0.3]);           // newer arrives last
```

### 3. Test each layer at its own level

- **Domain/service rules:** call the service directly. `UsagePlane` in `tests/controller_protocol.rs` builds `ProtocolService` with a `ManualClock` and a temp root, no `Harness::start`. Validation, replace, and expiry tests run there.
- **HTTP binding of one route:** test it in the server's `http` module with `tower::ServiceExt::oneshot` on a tiny `Router` that uses the same layer. Extract the layer to a function (`usage_body_limit()`) so the route and the test share it. Check the error mapping with `HttpProtocolError(..).into_response()`.
- **Output of a CLI command:** split the command into a thin `print!` wrapper and a pure function that returns `String` and takes `now` as a parameter (`usage_output`). Test the function with a double.
- **TUI state without a list server:** load the list with `TaskApp::load(listing(..))` instead of reading it from a `Stub` (`app_reading_with_list`).

Keep one end-to-end test only where the wiring itself is the risk; do not repeat a service rule through HTTP.

### 4. Time is a parameter

Use `ManualClock` (`clock.advance(..)`) for server rules and pass `now` to output functions. Build test data from the clock (`claude_report(&plane.clock, ..)`), so recorded fixtures do not go stale relative to validation rules such as "`resets_at` must not be in the past".

## Why This Matters

- Tests run fast and do not depend on free ports or socket timing.
- A gate in a double makes in-flight states (order, single send, hang, no second read) testable; with a real server these states depend on scheduling and are flaky.
- Each rule is tested once, at the layer that owns it, so a change in a rule breaks one test, not three.
- The seam also makes the production code clearer: the `Session` loop, the TUI, and the CLI depend on one call, not on a full client.

## When to Apply

- New code that talks to the control plane from the runner, TUI, or CLI.
- A test that needs `TcpListener`, `Stub::start`, or `Harness::start` only to answer one request.
- A test that uses `sleep` to wait for a request: replace it with an arrived signal plus `timeout`.

Do not apply when the test is about the transport itself (auth headers, reconnect, real serialization over the wire); keep those as a small set of integration tests.

## Examples

Before (TUI test):

```rust
let stub = Stub::start().await;
let usage = UsageDouble::answering(claude_usage());
let mut app = app_with_list(&stub).await
    .with_usage_source(Arc::clone(&usage) as Arc<dyn UsageSource>);
```

After:

```rust
let usage = UsageDouble::answering(claude_usage());
let mut app = app_reading_with_list(&usage); // TaskApp::load(listing(..)), no server
```

Before (CLI): `usage()` called `client.control_usage()` and `println!` directly, so a test needed a server and captured stdout. After: `usage()` only prints `usage_output(&client, json, Utc::now())`; tests call `usage_output(&double, .., fixed_now)` and compare the string.

## Related

- `docs/solutions/design-patterns/latest-value-reports-coalesce-per-key-one-send-at-a-time-2026-10-31.md` (the forwarder these runner tests check)
- `docs/solutions/design-patterns/tui-reads-as-keyed-background-jobs-with-loading-marker-2026-10-22.md` (TUI background reads that `UsageDouble` drives)
