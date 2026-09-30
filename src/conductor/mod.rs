//! The conductor: a module of the daemon that works beside an execution
//! (D55).
//!
//! # Why inside the daemon
//!
//! The triggers that matter most — a step with no usable output label (F1),
//! a step that failed for good (F3) — are internal to the engine and must
//! survive a restart. A separate client would have to recover them from the
//! SSE stream, which has no replay. So the conductor lives in the daemon's
//! process and calls the same public [`crate::engine::Engine`] methods the
//! API routes do; an operator sees it only through the API (KTD1).
//!
//! # Layout
//!
//! * [`store`] — the conductor's own files of one execution: state, chat,
//!   annotations (KTD2).
//! * [`actor`] — one actor per execution, the only writer of those files,
//!   with its two-priority durable queue (KTD11), R24's one repeat of a
//!   failed automatic invocation, and the seam ([`ConductorInvoker`]) the
//!   actual invocation plugs into.
//! * [`invoke`] — the invocation itself on the default harness, read-only
//!   (KTD5), built from a [`snapshot`] of the execution and a [`prompt`],
//!   answered with a checked [`response`] (KTD6).
//! * [`rescue`] — the conductor's half of the label rescue (F1): the inline
//!   invocation, and its decisions saved before they are applied (KTD13).
//! * [`ConductorRegistry`] — the actors the daemon started, and the
//!   interrupts of advisory invocations (KTD10).
//!
//! # Restart
//!
//! [`ConductorRegistry::restore_at_startup`] is the conductor's half of the
//! daemon's startup hook, run after `persistence::recovery::recover`. It is
//! the only replay the daemon does, acceptable because an invocation only
//! reads. What it covers: an unanswered chat message is served again; an
//! interrupted advisory invocation loses its marker and runs again. A label
//! rescue (F1) is never run from here: the engine's pass that carries the
//! step on invokes the conductor again (see [`rescue`]), and what a rescue
//! left for an execution that no longer owes one is discarded. Step
//! failures (F3) found on disk are reported, not run (U11).

pub mod actor;
pub mod invoke;
pub mod prompt;
pub mod rescue;
pub mod response;
pub mod snapshot;
pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

pub use actor::{
    AdvisoryInterrupts, ConductorActor, ConductorAnswer, ConductorInvoker, Invocation,
    InvocationOutcome, InvocationQueue, UnavailableInvoker, CONDUCTOR_UNAVAILABLE,
    MAX_AUTOMATIC_ATTEMPTS,
};
pub use invoke::{ConductorAccess, HarnessInvoker, DEFAULT_CONDUCTOR_TIMEOUT};
pub use response::{
    allowed_decisions, proposals_allowed, ConductorResponse, Decision, DecisionKind, ProposalKind,
    ProposedAction, ResponseError,
};
pub use snapshot::{Snapshot, SnapshotBudget, SnapshotInput};
pub use store::{
    Annotation, ChatAuthor, ChatEntry, ChatEntryKind, ConductorState, ConductorStore,
    PendingInvocation, QueuedRequest, SavedDecision, CONDUCTOR_STATE_VERSION,
};

use crate::domain::{ConductorTrigger, ExecutionId, ExecutionStatus};
use crate::engine::{owed_output_rescue, Engine};
use crate::persistence::Result;

/// The conductor actors of one daemon, one per execution, started on
/// demand.
pub struct ConductorRegistry {
    engine: Arc<Engine>,
    invoker: Arc<dyn ConductorInvoker>,
    advisory: Arc<AdvisoryInterrupts>,
    actors: Mutex<HashMap<ExecutionId, Arc<ConductorActor>>>,
}

/// What [`ConductorRegistry::restore_at_startup`] found and did.
#[derive(Debug, Default)]
pub struct ConductorRestore {
    /// Executions whose queue was served again, with how many requests.
    pub requeued: Vec<(ExecutionId, usize)>,
    /// Automatic requests left on disk for their own restart rows
    /// (U10, U11), per execution.
    pub parked: Vec<(ExecutionId, Vec<QueuedRequest>, Option<PendingInvocation>)>,
    /// Executions whose leftover label rescue was discarded: they no longer
    /// owe one.
    pub discarded: Vec<ExecutionId>,
}

impl ConductorRegistry {
    /// A registry whose actors run the conductor through `invoker`.
    pub fn new(engine: Arc<Engine>, invoker: Arc<dyn ConductorInvoker>) -> Self {
        Self {
            engine,
            invoker,
            advisory: Arc::new(AdvisoryInterrupts::default()),
            actors: Mutex::new(HashMap::new()),
        }
    }

    /// The actor of `execution_id`, opened (and its queue rebuilt from
    /// disk) the first time it is asked for.
    pub fn actor(&self, execution_id: &str) -> Result<Arc<ConductorActor>> {
        let mut actors = self.lock();
        if let Some(actor) = actors.get(execution_id) {
            return Ok(Arc::clone(actor));
        }
        let actor = Arc::new(ConductorActor::open(
            Arc::clone(&self.engine),
            Arc::clone(&self.invoker),
            Arc::clone(&self.advisory),
            execution_id,
        )?);
        actors.insert(execution_id.to_string(), Arc::clone(&actor));
        Ok(actor)
    }

    /// The conductor's files of `execution_id`, for reading.
    pub fn store(&self, execution_id: &str) -> ConductorStore {
        ConductorStore::new(self.engine.store().paths(), execution_id)
    }

    /// Stops the advisory invocation running for `execution_id` (KTD10).
    /// Returns whether there was one.
    pub fn interrupt_advisory(&self, execution_id: &str) -> bool {
        self.advisory.interrupt(execution_id)
    }

    /// The registry of advisory interrupts, separate from the API's
    /// `active_tasks`.
    pub fn advisory_interrupts(&self) -> &AdvisoryInterrupts {
        &self.advisory
    }

    /// The conductor's rows of the daemon restart matrix: opens the actor of
    /// every execution that has conductor work on disk and starts serving
    /// it. Run once, after `persistence::recovery::recover`.
    ///
    /// An execution whose conductor state cannot be read is skipped with a
    /// warning rather than failing the whole start, like the rest of the
    /// startup sweep.
    pub fn restore_at_startup(&self) -> Result<ConductorRestore> {
        let mut restore = ConductorRestore::default();

        for execution_id in self.engine.store().list_execution_ids()? {
            if !self.store(&execution_id).exists() {
                continue;
            }
            let actor = match self.actor(&execution_id) {
                Ok(actor) => actor,
                Err(error) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %error,
                        "conductor: could not restore the conductor's state"
                    );
                    continue;
                }
            };

            let queued = actor.queued();
            if queued > 0 {
                actor.start();
                restore.requeued.push((execution_id.clone(), queued));
            }
            if self.leaves_stale_rescue(&actor) {
                match self.discard_rescue(&execution_id) {
                    Ok(_) => restore.discarded.push(execution_id.clone()),
                    Err(error) => tracing::warn!(
                        execution_id = %execution_id,
                        error = %error,
                        "conductor: could not discard a stale label rescue"
                    ),
                }
            }
            let (requests, pending) = actor.parked_automatic();
            if !requests.is_empty() || pending.is_some() {
                restore.parked.push((execution_id, requests, pending));
            }
        }

        Ok(restore)
    }

    /// Whether `actor` holds something of a label rescue — a marker, a
    /// queued request, an unapplied decision — for an execution that no
    /// longer owes one: it is not `Running`, or has no rescue record at its
    /// current state (the step moved on, or never got that far).
    fn leaves_stale_rescue(&self, actor: &ConductorActor) -> bool {
        let state = actor.state();
        let is_rescue = |request: &QueuedRequest| request.trigger == ConductorTrigger::OutputRescue;
        let holds_rescue = state.queue.iter().any(is_rescue)
            || state
                .pending
                .as_ref()
                .is_some_and(|pending| is_rescue(&pending.request))
            || !state.unapplied_decisions.is_empty();
        if !holds_rescue {
            return false;
        }
        match self.engine.store().load_execution(actor.execution_id()) {
            Ok(execution) => {
                execution.status != ExecutionStatus::Running
                    || !matches!(owed_output_rescue(&execution), Ok(Some(_)))
            }
            // An execution that cannot be read is left as it is.
            Err(_) => false,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ExecutionId, Arc<ConductorActor>>> {
        self.actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
