//! The commands a controller long-polls for and acknowledges.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use super::long_poll::Checked;
use crate::controller_protocol::credentials::ControllerIdentity;
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::service::ProtocolService;
use crate::controller_protocol::wire::*;
use crate::domain::{TaskId, TaskLaunchSpec};
use crate::orchestration::{RunnerState, TaskRecord};

impl ProtocolService {
    /// The backend's desired state for this controller, read under its
    /// unexpired lease, as a long-poll (KTD1). A full read (no `since`, or
    /// a cursor this control plane does not know) answers at once with
    /// every task. An incremental read answers as soon as a task's desired
    /// state differs from what the response at `since` delivered — or one
    /// left the desired state, which is answered with a `Forget` — and
    /// empty after `query.wait_secs`. A reader lease running out changes a
    /// task's desired state with no mutation: the wait ends then, too.
    pub async fn wait_for_commands(
        &self,
        identity: &ControllerIdentity,
        query: CommandsQuery,
    ) -> ProtocolResult<CommandsResponse> {
        let wait = self.long_poll(query.wait_secs);
        let found = self
            .wait_for_or_until(wait, None, || {
                self.read_commands(identity, query.since, true)
            })
            .await?;
        match found {
            Some(response) => Ok(response),
            None => match self.read_commands(identity, query.since, false)? {
                Checked::Found(response) => Ok(response),
                Checked::NotYet { .. } => unreachable!("a read that does not wait answers"),
            },
        }
    }

    /// One read of the desired state against what was delivered at `since`.
    /// With `wait_for_change`, an incremental read that finds nothing new
    /// answers `NotYet`, with the earliest reader-lease expiry to look again
    /// at.
    fn read_commands(
        &self,
        identity: &ControllerIdentity,
        since: Option<u64>,
        wait_for_change: bool,
    ) -> ProtocolResult<Checked<CommandsResponse>> {
        let desired = self.desired_state(identity)?;
        let controller = &identity.controller_id;
        let Some(delivered) = self
            .deliveries
            .delivered(controller, since, desired.lease_epoch)
        else {
            let cursor = self
                .deliveries
                .record(controller, desired.lease_epoch, &desired.commands);
            return Ok(Checked::Found(CommandsResponse {
                lease_epoch: desired.lease_epoch,
                cursor,
                full: true,
                commands: desired.commands,
            }));
        };
        let changed = changes(&delivered, &desired.commands);
        if changed.is_empty() && wait_for_change {
            let recheck_at = desired.next_reader_expiry.map(|until| {
                let left = (until - self.control.now())
                    .to_std()
                    .unwrap_or_default()
                    .max(MIN_RECHECK);
                tokio::time::Instant::now() + left
            });
            return Ok(Checked::NotYet { recheck_at });
        }
        let cursor = self
            .deliveries
            .record(controller, desired.lease_epoch, &desired.commands);
        Ok(Checked::Found(CommandsResponse {
            lease_epoch: desired.lease_epoch,
            cursor,
            full: false,
            commands: changed,
        }))
    }

    /// Every task of the controller's backend it should act on, as of now.
    fn desired_state(&self, identity: &ControllerIdentity) -> ProtocolResult<DesiredState> {
        let lease = self.require_lease(identity)?;
        // Decide admission before answering: the controller asking is the
        // proof its backend is online, and its limit is read fresh.
        self.reschedule(&identity.backend_id);
        let now = self.control.now();
        let tasks = self.control.list_tasks()?;
        let ours = || {
            tasks
                .iter()
                .filter(|record| record.assignment.backend_id == identity.backend_id)
        };
        let next_reader_expiry = ours()
            .filter(|record| record.reader_lease_active(now))
            .filter_map(|record| record.reader_lease.as_ref().map(|lease| lease.until))
            .min();
        let commands = ours()
            // A queued task is offered only once the scheduler admitted it.
            .filter(|record| {
                record.runner_state != RunnerState::Queued || record.reservation.is_some()
            })
            .filter_map(|record| desired_command(record, now))
            .collect();
        Ok(DesiredState {
            lease_epoch: lease.epoch,
            commands,
            next_reader_expiry,
        })
    }

    /// Records acknowledgements; duplicates and reorderings are harmless.
    pub fn acknowledge(
        &self,
        identity: &ControllerIdentity,
        request: AckRequest,
    ) -> ProtocolResult<AckResponse> {
        let mut acks = Vec::with_capacity(request.acks.len());
        for ack in &request.acks {
            let record = self.backend_task(identity, &ack.task_id)?;
            if ack.generation > record.generation {
                return Err(ProtocolError::bad_request(
                    "invalid_ack",
                    format!(
                        "task '{}' is at generation {}, cannot acknowledge {}",
                        record.task_id, record.generation, ack.generation
                    ),
                ));
            }
            acks.push((record.task_id, ack.generation));
        }
        let controller = self
            .credentials
            .acknowledge(&identity.controller_id, &acks)?
            .ok_or(ProtocolError::Unauthorized)?;
        Ok(AckResponse {
            acknowledged: acks
                .iter()
                .map(|(task_id, _)| CommandAck {
                    task_id: task_id.to_string(),
                    generation: controller
                        .acknowledged
                        .get(task_id.as_str())
                        .copied()
                        .unwrap_or(0),
                })
                .collect(),
        })
    }

    /// The sealed launch spec of a task of the controller's backend, for the
    /// task's current `generation`, to the lease holder only (R2). A
    /// generation the task moved past is refused with `stale_generation`:
    /// the controller re-reads its commands. The spec is never logged.
    pub fn launch_spec(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
        generation: u64,
    ) -> ProtocolResult<TaskLaunchSpec> {
        let record = self.backend_task(identity, task_id)?;
        self.require_lease(identity)?;
        if generation != record.generation {
            return Err(ProtocolError::conflict(
                "stale_generation",
                format!(
                    "task '{}' is at generation {}, not {generation}",
                    record.task_id, record.generation
                ),
            ));
        }
        Ok(record.launch_spec)
    }

    /// A task of the controller's backend, or `NotFound` (R37 scope).
    pub(super) fn backend_task(
        &self,
        identity: &ControllerIdentity,
        task_id: &str,
    ) -> ProtocolResult<TaskRecord> {
        let task_id = TaskId::parse(task_id).map_err(|_| ProtocolError::NotFound)?;
        let record = self.control.task(&task_id)?;
        if record.assignment.backend_id != identity.backend_id {
            return Err(ProtocolError::NotFound);
        }
        Ok(record)
    }
}

/// Least wait before a reader-lease expiry is looked at again: a clock a
/// hair behind the timer must not spin the long-poll.
const MIN_RECHECK: std::time::Duration = std::time::Duration::from_millis(10);

/// The desired state of a backend as one read saw it.
struct DesiredState {
    lease_epoch: u64,
    /// One command per task the controller acts on, in the order of the
    /// task list.
    commands: Vec<DesiredCommand>,
    /// When the first active reader lease of the backend runs out.
    next_reader_expiry: Option<DateTime<Utc>>,
}

/// The commands of `current` that differ from what was `delivered`, and a
/// `Forget` — the last delivered command, with that action — for each
/// delivered task no longer in it.
fn changes(
    delivered: &BTreeMap<TaskId, DesiredCommand>,
    current: &[DesiredCommand],
) -> Vec<DesiredCommand> {
    let live: HashSet<&TaskId> = current.iter().map(|command| &command.task_id).collect();
    let changed = current
        .iter()
        .filter(|command| delivered.get(&command.task_id) != Some(*command))
        .cloned();
    let gone = delivered
        .values()
        .filter(|command| !live.contains(&command.task_id))
        .map(|command| DesiredCommand {
            action: DesiredAction::Forget,
            ..command.clone()
        });
    changed.chain(gone).collect()
}

/// The desired state of one task as of `now`; `None` once it is archived.
/// It holds only what the controller acts on (KTD1): `reader_needed` is the
/// reader lease as it stands now, so a lease that ran out stops being asked
/// for without waiting for the periodic pass to remove it; the runner's
/// progress leaves out its event cursor and timestamps, so a heartbeat or a
/// progress snapshot changes no command.
fn desired_command(record: &TaskRecord, now: DateTime<Utc>) -> Option<DesiredCommand> {
    let action = match record.runner_state {
        RunnerState::Queued => DesiredAction::Start,
        RunnerState::Provisioning => DesiredAction::Provision,
        RunnerState::Running => DesiredAction::Run,
        RunnerState::PauseRequested => DesiredAction::PauseAtSafePoint,
        RunnerState::Stopped => DesiredAction::KeepPaused,
        RunnerState::Interrupted | RunnerState::Unknown => DesiredAction::Hold,
        RunnerState::Archiving => DesiredAction::Archive,
        RunnerState::Archived => return None,
    };
    Some(DesiredCommand {
        task_id: record.task_id.clone(),
        generation: record.generation,
        runner_state: record.runner_state,
        action,
        resource_key: record.assignment.resource_key.clone(),
        launch_digest: record.launch_spec.digest.clone(),
        runner: record.runner.as_ref().map(|runner| RunnerProgress {
            incarnation: runner.incarnation.clone(),
            launch_generation: runner.launch_generation,
            safe_point_acknowledged: runner.safe_point_generation == Some(record.generation),
        }),
        reader_needed: record.reader_lease_active(now),
    })
}

/// The desired state delivered to each controller, in memory only (KTD1):
/// a control plane that restarted knows no cursor and answers the next read
/// in full, which is always safe.
///
/// A delivery is kept under the cursor of its response, and an incremental
/// read is answered against the one its `since` names — so a response the
/// controller never received (a dropped long-poll) costs nothing: it asks
/// again from the last one it applied. The last few are kept per controller.
pub(in crate::controller_protocol::service) struct Deliveries {
    next_cursor: AtomicU64,
    by_controller: Mutex<HashMap<String, VecDeque<Delivery>>>,
}

/// How many deliveries are kept per controller: the one it applied last and
/// a few answered since that it may not have received.
const KEPT_DELIVERIES: usize = 4;

struct Delivery {
    cursor: u64,
    lease_epoch: u64,
    commands: Arc<BTreeMap<TaskId, DesiredCommand>>,
}

impl Default for Deliveries {
    fn default() -> Self {
        // Cursors of an earlier run of the control plane must not name a
        // delivery of this one: they start somewhere new each time.
        let start = uuid::Uuid::new_v4().as_u64_pair().0 >> 2;
        Self {
            next_cursor: AtomicU64::new(start),
            by_controller: Mutex::default(),
        }
    }
}

impl Deliveries {
    /// What the response at `since` delivered to `controller`, if it was
    /// answered under this `lease_epoch` and is still kept. Deliveries
    /// before it are dropped: the controller applied a later one.
    fn delivered(
        &self,
        controller: &str,
        since: Option<u64>,
        lease_epoch: u64,
    ) -> Option<Arc<BTreeMap<TaskId, DesiredCommand>>> {
        let since = since?;
        let mut by_controller = self.by_controller.lock().expect("deliveries");
        let kept = by_controller.get_mut(controller)?;
        let at = kept
            .iter()
            .position(|delivery| delivery.cursor == since && delivery.lease_epoch == lease_epoch)?;
        kept.drain(..at);
        Some(Arc::clone(&kept[0].commands))
    }

    /// Keeps `commands` as delivered to `controller`; the response's cursor.
    fn record(&self, controller: &str, lease_epoch: u64, commands: &[DesiredCommand]) -> u64 {
        let cursor = self.next_cursor.fetch_add(1, Ordering::Relaxed);
        let commands = commands
            .iter()
            .map(|command| (command.task_id.clone(), command.clone()))
            .collect();
        let mut by_controller = self.by_controller.lock().expect("deliveries");
        let kept = by_controller.entry(controller.to_string()).or_default();
        kept.push_back(Delivery {
            cursor,
            lease_epoch,
            commands: Arc::new(commands),
        });
        while kept.len() > KEPT_DELIVERIES {
            kept.pop_front();
        }
        cursor
    }
}
