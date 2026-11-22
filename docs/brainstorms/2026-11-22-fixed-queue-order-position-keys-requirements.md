---
date: 2026-11-22
topic: fixed-queue-order-position-keys
---

# Fixed queue order with sparse position keys

## Summary

A task must never change its place in the queue by itself. Only the owner
changes the order (with `move`). Today a task that waited for the owner and
gets an answer when no slot is free goes to the **front** of the queue. This
is a bug. After this change the task goes back to the queue at the place it
always had.

To make this simple and cheap, the dense `rank` (1, 2, 3, ...) is replaced by
a sparse **position key**: an integer made from the exact date and time the
task was created. A move gives the moved task the rounded mean of the keys of
its two new neighbours. When there is no free integer between them, a small
number of nearby tasks get new keys (rebalance). A move changes the keys of at
most a few tasks, not of the whole queue. Archived tasks are not part of the
order.

---

## Problem Frame

The queue of one backend is its tasks in `rank` order
(`crates/narsor-engine/src/orchestration/scheduler.rs`). The scheduler admits
the waiting (`Queued`, no reservation) tasks by `rank`, lowest first.

Requirement R20 of
`docs/plans/2026-10-18-002-feat-zwalnianie-slotow-i-runner-do-odczytu-plan.md`
says: a resume of a `Stopped` task on a full backend puts the task at the
front of the queue. The code does it in two steps:

- `ControlPlane::command_routed` sets `record.rank = 0` and sends the task to
  `Queued`.
- `ControlPlane::put_first_in_queue` gives the task rank 1 and renumbers all
  other tasks of the backend behind it.

The resume comes from the owner's `Resume`, and also from work that the owner
sends to a paused task: an answer to a question, a comment, an operator
command (`resume_for_work` in
`crates/narsor-engine/src/controller_protocol/service/operator/input.rs`).
Thus, an answer to a task that waits for the owner makes the task jump over
all tasks that the owner put in front of it. The owner did not ask for this,
and the owner loses the order that the owner set.

The dense `rank` also has a cost: each move renumbers every task between the
old and the new place, and `put_first_in_queue` renumbers the whole backend.
Each renumbering is many record writes under the schedule lock, and a crash
in the middle leaves equal ranks that a later pass must repair
(`dense_backend_tasks`).

---

## Actors

- **Owner** — sets the queue order (CLI `task move`, TUI, web). Answers
  questions of tasks, pauses and resumes tasks.
- **Scheduler** (`ControlPlane::schedule`) — admits waiting tasks in queue
  order when a slot is free.
- **Policy** (limit stop) — stops a running task when the global limit goes
  down. The stopped task goes back to the queue "on its own rank".

---

## Key Flows

**F1. Answer to a task when no slot is free (the bug).**
Queue of backend B: T1 (running), T2 (stopped, waits for the owner), T3, T4
(queued). The limit is full. The owner answers T2. T2 goes to `Queued`. Its
position key does not change, so T2 is before T3 and T4 (it was before them).
If the owner had moved T2 after T4 earlier, T2 stays after T4.

**F2. Move between two tasks.**
The owner moves T5 between T2 (key 1000) and T3 (key 2000). T5 gets key
`floor((1000 + 2000) / 2) = 1500`. No other task changes.

**F3. Move with no free key.**
T2 has key 1000 and T3 has key 1001. The owner moves T5 between them. There
is no integer strictly between 1000 and 1001, so the rebalance (R7) first
moves T2 to the middle between its predecessor and 1000. If this makes a gap,
T5 goes into it.

**F4. Move to the top or the bottom.**
Top: T5 goes between a virtual predecessor with key 0 and the first task.
Bottom: T5 goes between the last task and a virtual successor whose key is
the current date and time.

---

## Requirements

### Order

- **R1.** The queue order of a backend is the order of the position keys of
  its non-archived tasks, lowest first. Archived tasks have no place in the
  order: they are not neighbours in a move, and they are not counted in the
  position that a client shows.
- **R2.** No transition of a task changes its position key. This includes:
  resume of a `Stopped` task with or without a free slot, resume caused by an
  answer, comment or operator command, the limit stop and its return to the
  queue, a retry or auto-resume, admission, and every other state change.
  Only the owner's move (R5–R8), creation (R3) and the rebalance (R7) write a
  position key.
- **R3.** A new task gets the position key made from its creation date and
  time as an integer (see Assumption A1). The key must be unique and greater
  than every key on the backend: when the clock gives a value that is not
  greater (two tasks in the same time unit, or the clock went back), the key
  is the current maximum + 1.
- **R4.** A task that comes back after a pause, a stop or a wait for the
  owner keeps its key, and so keeps its place relative to all other
  non-archived tasks of the backend.

### Move

- **R5.** A move of task X between tasks P (before) and N (after) gives X the
  key `floor((key(P) + key(N)) / 2)` (integer division, Assumption A2), if
  this value is strictly between
  `key(P)` and `key(N)`. X is taken out of the order before P and N are found.
- **R6.** Boundaries: when X goes to the first place, P is a virtual task
  with key 0. When X goes to the last place, N is a virtual task whose key is
  the current date and time in the same unit as R3 (if this value is not
  greater than `key(P)`, then N = `key(P)` + 2, so X gets `key(P)` + 1). The
  existing targets `Before(anchor)`, `After(anchor)`, `Top` and `Bottom`
  keep their meaning.
- **R7. Rebalance.** When no integer is strictly between `key(P)` and
  `key(N)`, the system makes space by moving nearby tasks away from the gap,
  in this order, until the gap between P and N has a free integer:
  1. P (index −1) goes to the middle between its predecessor and its current
     key.
  2. N (index +1) goes to the middle between its current key and its
     successor.
  3. Index −2, then −1 again: each goes to the middle between its predecessor
     and its current key.
  4. Index +2, then +1 again: each goes to the middle between its current key
     and its successor.
  5. And so on with −3…−1, +3…+1, until the gap has space.
  The boundaries of R6 (0 before the first task, the current time after the
  last task) apply to the rebalance too. A task whose move gives no new key
  (no space on its side) keeps its key, and the procedure continues.
- **R8.** If the rebalance reaches both ends of the order and still finds no
  space, the move fails with a clear error and changes nothing. (With a
  64-bit key and creation times as keys, this is not expected in practice.)
- **R9.** Each single key write keeps a strict order of all tasks: a task
  moves only inside the gap between its current neighbours. Thus a crash in
  the middle of a rebalance leaves a valid order. It can leave a partly made
  space, but never two tasks with the same key and never a changed order.
  The "current neighbours" of a key write come from the full order of the
  backend, which includes the moved task X at its old key until X's own final
  write (R10). X is left out of the order only to find P and N (R5), not to
  bound the rebalance steps.
- **R10.** A move and its rebalance run under the backend's schedule lock,
  as today. The moved task's key is written last, with the `Moved` event as
  the single commit point. If the move is refused at the end (a command came
  first), the neighbours keep their new keys: this is allowed because R9
  keeps their order.
- **R11.** A move that does not change the order writes and announces
  nothing, as today. Only a `Queued` task can be moved, as today.

### Admission and display

- **R12.** The scheduler admits waiting tasks by position key, lowest first.
  The tie-breaker for equal keys (only legacy data can have them) is
  `queue_sequence`.
- **R13.** Clients (CLI `#N`, TUI, web) keep showing a 1-based position. The
  server computes it from the key order of the non-archived tasks of the
  backend. The `Moved` event keeps its `order` list. Clients that worked
  with `rank` keep working (see Assumption A5).

### Migration

- **R14.** Existing records get position keys in their current queue order
  (current `rank`, then `queue_sequence`). The migration keeps the order that
  the owner set. A task whose creation time is greater than the key of its
  predecessor gets its creation time as key. Otherwise it gets the key of its
  predecessor + 1. Archived tasks keep a key, but it has no effect on the
  order.
- **R15.** The migration is idempotent and safe to stop and run again, like
  the existing rank migration (`migrate_backend` in
  `crates/narsor-engine/src/orchestration/store.rs`).

### Removed behaviour

- **R16.** The "front of the queue" resume (R20 of the 2026-10-18 plan) is
  removed: `rank = 0` in `command_routed`, `put_first_in_queue`, and the doc
  comments that describe it. The dense renumbering for equal ranks
  (`dense_backend_tasks`, `normalise_ranks`) is no longer needed for order
  repair.

---

## Acceptance Examples

- **AE1 (F1).** Limit 1. T1 runs. T2 is `Stopped` and waits for the owner. T3
  and T4 are queued, T2 is before them. The owner answers T2. Result: T2 is
  `Queued`. The order is T2, T3, T4. When T1 frees its slot, T2 starts.
- **AE2.** Same as AE1, but the owner earlier moved T3 and T4 before T2. The
  owner answers T2. Result: the order is T3, T4, T2. T3 starts first.
- **AE3.** The owner moves T5 between keys 1000 and 2000. T5 gets 1500. The
  records of all other tasks do not change.
- **AE4.** Keys: A = 500, P = 1000, N = 1001, B = 3000. The owner moves X
  between P and N. P gets 750 (the middle between A and P). X gets
  `floor((750 + 1001) / 2)` = 875 (rounding down, Assumption A2). Only P and
  X change.
- **AE5.** An archived task Z is between P and N in key order. The owner
  moves X "after P". Z is not a neighbour: X goes between P and N.
- **AE6.** The limit goes down, and running task T stops at a safe point.
  T goes back to `Queued` with its old key.

---

## Success Criteria

- No path from `Stopped`, a wait for the owner, or a limit stop to `Queued`
  changes the queue order.
- A typical move writes one record (the moved task). A move into a full gap
  writes the moved task and a few neighbours, not the whole backend.
- Existing installations keep the order that the owner set before the
  upgrade.
- `cargo test --workspace` passes. Tests cover: AE1/AE2 (no jump), AE3
  (single write), AE4 (rebalance), migration keeps the order.

---

## Scope Boundaries

- The order stays per backend, as today. A global order across backends is
  not in scope.
- No new move targets (for example "move by N places") and no changes to the
  CLI/TUI/web move UI.
- Observer queues (`observer-queues/`, decision gates) are a different
  mechanism and are not in scope.
- Admission rules (limits, reservations, non-preemption) do not change. Only
  the order key changes.

---

## Key Decisions

- **Sparse integer keys instead of dense ranks.** Proposed by the user. A
  move writes O(1) records in the usual case, and an interrupted move can not
  make an invalid order (R9).
- **Creation time as the initial key.** Proposed by the user. New tasks go to
  the end without a read of the other tasks' ranks, and the "bottom" bound
  (the current time) is always after every task that exists.
- **Archived tasks are out of the order.** Proposed by the user. They do not
  take key space and they do not change the positions that clients show.
- **The R20 "front of the queue" rule is reversed.** The user calls it a bug:
  the queue order belongs to the owner, and a task must not jump.

---

## Assumptions to confirm

- **A1. Unit of the key.** The key is the creation time in **microseconds**
  since the Unix epoch, stored as a 64-bit integer. Microseconds give more
  space between tasks than milliseconds or seconds, so a rebalance is less
  frequent, and a 64-bit integer holds this value for a very long time. The
  user said "exact date with time", which permits this unit.
- **A2. Rounding of the mean.** The mean is rounded down (integer division).
  If the result is not strictly between the two keys, the gap is full and the
  rebalance (R7) runs. The user said "rounded to integers" without a rule;
  any rule works, because R5 checks the result.
- **A3. Interpretation of the rebalance order.** R7 is a reading of "the task
  before, then the task after, then −2 and −1, then +2 and +1, and so on".
  After each step the procedure checks the gap between P and N and stops as
  soon as it has a free integer. The aim is to change as few records as
  possible.
- **A4. Which tasks are neighbours.** All non-archived tasks of the backend
  are in the order, also running, stopped and finished ones (today they keep
  their `rank` too). A move "before T" or "after T" can use any of them as an
  anchor, as today. Only archived tasks are out (user's rule).
- **A5. Wire compatibility.** The `rank` field in `TaskSummary` and in the
  `Moved` event stays and means "1-based position among the non-archived
  tasks of the backend", computed by the server. The position key can be
  added as a new field. Old clients keep working.
- **A6. The migration keeps the current order.** The user's rule ("key =
  creation time") applied directly to old tasks would reset every move that
  the owner made. The migration therefore keeps the current order (R14). For
  a queue that the owner never reordered, the result is equal to the creation
  time. Where the owner moved a task, the tasks behind a task with a later
  creation time get its key + 1, + 2, and so on (R14).
- **A7. Archived tasks are not restored to the order.** An archived task
  cannot come back today (the tombstone is permanent). If this changes, the
  task returns with its old key (R4).
- **A8. A refused move may leave rebalanced neighbours.** R10 accepts that
  neighbours keep their new keys when the final write fails. Their order does
  not change, so the owner sees no difference.

---

## Open questions for the user

None. Every point that was not clear is in "Assumptions to confirm".

---

## Outstanding Questions

### Deferred to Planning

- Exact field name and type of the key in `TaskRecord` (for example
  `position: i64`), and the record version bump.
- Whether `queue_sequence` is still needed after the change (as a
  tie-breaker for legacy data only), or can be removed later.
- Where the position shown to clients (A5) is computed for the `Moved`
  event: today the event holds the rank of the whole backend order.
