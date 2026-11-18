---
title: "Gate queue entries must outlive runner problems, restores and unreadable files"
date: 2026-11-18
category: logic-errors
module: engine/observers/queue-entries
problem_type: logic_error
component: background_job
symptoms:
  - "A runner that reports a problem makes the control plane end the watched gate episode and remove its queue entry"
  - "An entry that comes back after a restore starts at the restore time, so scripts miss a review done since the gate began"
  - "One corrupt or newer-version entry file fails the claim of every queue, or the tick retries it forever and the gate is never served"
root_cause: logic_error
resolution_type: code_fix
severity: high
tags: [observers, queue-entries, runner-snapshot, gate-episode, began-at, corrupt-file, decide]
---

# Gate queue entries must outlive runner problems, restores and unreadable files

## Problem
A human `decision` gate with a `watch` block has one queue entry per wait episode, keyed `(task, episode)`. The control plane keeps the entry in step with the runner snapshots. Three paths ended or damaged the entry when the gate was still open.

## Symptoms
- A runner in a problem state sends its last execution id with no pending input. The snapshot handler read that as "the gate ended" and removed the entry, its start and its single warning.
- An entry added back (last observer of the queue deleted and a new one added, or a crash between two writes) started at the restore time. The example scripts then ignored a review done since the gate began.
- One unreadable entry file (corrupt, or of a newer record version) made `list_queue_entries` fail for all owners. A round failed on it at every claim. The tick tried to add the same entry at every tick and the gate was never served.

## What Didn't Work
- Treating "execution id and no pending input" as a gate exit. This is also what a runner with a problem sends.
- Starting a re-added entry at `now`. The episode start is the only value that is true.
- Letting `list_queue_entries` return the first read error. All owners share the directory, so one bad file stopped every queue.

## Solution
- Follow the watched gate only when the snapshot has an execution id and no problem. A problem or a starting runner says nothing about the gate (R17).
- Store `began_at` (the time the control plane first saw the episode) on the watched gate. Use it as the entry start when the entry is added back.
- Log and skip an unreadable entry file in `list_queue_entries`. A round skips such an entry and is saved.
- At the next tick of an `on_episodes` observer, rewrite the corrupt entry of a waiting task from the watched gate. Never rewrite a file of a newer version. Remove unreadable files of episodes where no task waits.
- Keep an entry whose task waits but whose runner has a problem. Do not run it. Refuse an answer in that state with `gate_unavailable`, not `stale_episode`: the gate was neither answered nor left.

```rust
// crates/narsor-engine/src/orchestration/runner_state.rs
let gate_change =
    if snapshot.execution_cursor.execution_id.is_some() && snapshot.problem.is_none() {
        follow_watched_gate(record, &snapshot, now)
    } else {
        GateChange::default()
    };
```

## Why This Works
The entry follows the gate, not the runner health. Only two facts end an episode: an answer, or a snapshot that shows the runner left the gate. Persisting `began_at` on the task record makes the entry start independent of when the entry file is written. Skipping bad files per entry limits the damage of one file to that file.

## Prevention
- Before you treat a snapshot field as absent, ask which states send it empty (starting, problem, restore). Write a test for each state followed by recovery of the same episode.
- Store the source time of derived records on the source record, so a rebuild keeps it.
- Any listing over a shared directory logs and skips one bad file. It does not fail the listing.
- Tests: problem snapshot then recovery of the same episode; entry added back keeps the episode start; bad file next to good ones; bad entry in a running round; newer-version file left alone; refusal code during a runner problem (`tests/integration/observers_queue_entries.rs`, `observers_decide.rs`).

## Related Issues
- `docs/solutions/logic-errors/observer-round-next-entry-claim-timestamp-reset-2026-11-15.md`
- PR glakartur/narsor#111; requirements R17 in `docs/brainstorms/2026-11-17-observer-queues-at-decision-gates-requirements.md`
