//! The conductor inside a task runner (U5).
//!
//! The conductor is the same module the daemon hosts for its own
//! executions ([`crate::conductor`]); what differs is what it runs on. The
//! runner has no catalogue and no live default harness: its conductor reads
//! the workflow snapshot of the launch spec and runs on the harness the
//! task pinned (R42) — or on the one its owner chose since, which is kept on
//! the storage so a new incarnation honours it.
//!
//! # Available or not, decided once
//!
//! A conductor needs its harness in the task's own environment: the base
//! image brings the binary, not Narsor. So the runner looks for it when it
//! starts. Found, the conductor is assembled and attached to the Engine;
//! not found, there is none, the snapshot says why
//! (`conductor_available = false`), and a step that fails goes to the owner
//! as it is, with no diagnosis and no proposals. The Engine is never
//! attached to a conductor that cannot run, so it never waits for one.
//!
//! # Passes the conductor starts
//!
//! The conductor's one retry of a failed step (R21) is a pass of its own.
//! It runs in [`RunnerPasses`], beside the pass the runner starts itself,
//! and counts as running work everywhere the runner asks: it keeps a pause
//! waiting for its step boundary, defers owner input, and is what an
//! interrupt drops.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::preflight::{find_on_path, is_executable};
use super::storage::TaskStorage;
use super::Session;
use crate::conductor::assembly::{assemble, restore_at_startup};
use crate::conductor::{
    ConductorRegistry, ConductorView, HarnessInvoker, LiveHarness, PassLauncher,
};
use crate::controller_protocol::ConductorReadRequest;
use crate::domain::{WorkflowDefinition, WorkflowExecution};
use crate::engine::{Engine, HarnessRegistry};

/// The conductor of one runner, or the reason there is none.
pub(super) struct ConductorHost {
    registry: Option<Arc<ConductorRegistry>>,
    unavailable: Option<String>,
    harness: Arc<LiveHarness>,
    passes: Arc<RunnerPasses>,
}

impl ConductorHost {
    /// Assembles the conductor of the task on `storage` over `engine`, when
    /// its harness can run here.
    ///
    /// The harness is the owner's choice kept on the storage, else
    /// `pinned_harness`, the one the task was created with.
    pub(super) fn assemble(
        engine: &Arc<Engine>,
        storage: &TaskStorage,
        pinned_harness: &str,
        definition: &Arc<WorkflowDefinition>,
    ) -> Self {
        let chosen = storage
            .conductor_harness()
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the owner's choice of the conductor's harness is unreadable");
                None
            })
            .unwrap_or_else(|| pinned_harness.to_string());
        let harness = Arc::new(LiveHarness::new(&chosen));
        let passes = Arc::new(RunnerPasses::new(Arc::clone(definition)));

        if let Some(reason) = harness_unavailable(&chosen) {
            tracing::info!(%reason, "no conductor runs for this task");
            return Self {
                registry: None,
                unavailable: Some(reason),
                harness,
                passes,
            };
        }
        let invoker = HarnessInvoker::new(
            engine.store().clone(),
            storage.root(),
            storage.conductor_work_dir(),
        )
        .with_live_harness(Arc::clone(&harness))
        .with_pinned_definition(Arc::clone(definition));
        let registry = assemble(
            engine,
            Arc::new(invoker),
            Some(Arc::clone(&passes) as Arc<dyn PassLauncher>),
        );
        Self {
            registry: Some(registry),
            unavailable: None,
            harness,
            passes,
        }
    }

    /// The conductor, or why there is none, worded for the owner.
    pub(super) fn registry(&self) -> Result<&Arc<ConductorRegistry>, String> {
        self.registry.as_ref().ok_or_else(|| {
            format!(
                "this task has no conductor: {}",
                self.unavailable.as_deref().unwrap_or("it was not started")
            )
        })
    }

    /// Why there is no conductor; `None` while there is one.
    pub(super) fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// The passes the conductor started.
    pub(super) fn passes(&self) -> &Arc<RunnerPasses> {
        &self.passes
    }

    /// Serves again what the conductor left unfinished (run after the
    /// runner's own recovery, like the daemon's startup hook).
    pub(super) fn restore_at_startup(&self) {
        if let Some(registry) = &self.registry {
            restore_at_startup(registry);
        }
    }
}

/// Why `harness` cannot run in this process's environment, when it cannot:
/// an unknown name, or a binary that is not there.
pub(super) fn harness_unavailable(harness: &str) -> Option<String> {
    let registry = HarnessRegistry::defaults();
    let adapter = match registry.get(harness) {
        Ok(adapter) => adapter,
        Err(error) => return Some(error.message),
    };
    let program = adapter.program();
    let found = if program.contains('/') {
        is_executable(Path::new(program))
    } else {
        std::env::var_os("PATH").is_some_and(|path| find_on_path(program, &path).is_some())
    };
    (!found).then(|| format!("the harness '{harness}' needs '{program}', which is not on PATH"))
}

/// The pass the conductor started (its one retry of a failed step), the
/// way the runner's own pass is kept: one at a time, because only one pass
/// holds the execution's claim.
pub(super) struct RunnerPasses {
    definition: Arc<WorkflowDefinition>,
    active: Mutex<Option<JoinHandle<()>>>,
    ended: Arc<Notify>,
}

impl RunnerPasses {
    fn new(definition: Arc<WorkflowDefinition>) -> Self {
        Self {
            definition,
            active: Mutex::new(None),
            ended: Arc::new(Notify::new()),
        }
    }

    /// Whether a pass of the conductor's is still running.
    pub(super) fn active(&self) -> bool {
        self.lock().as_ref().is_some_and(|pass| !pass.is_finished())
    }

    /// Takes the pass out, to stop it.
    pub(super) fn take(&self) -> Option<JoinHandle<()>> {
        self.lock().take()
    }

    /// Stops the pass, if one runs, without waiting for it to let go.
    pub(super) fn abort(&self) {
        if let Some(pass) = self.lock().as_ref() {
            pass.abort();
        }
    }

    /// Resolves once a pass of the conductor's has ended.
    pub(super) async fn ended(&self) {
        self.ended.notified().await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PassLauncher for RunnerPasses {
    fn definition(
        &self,
        _execution: &WorkflowExecution,
    ) -> std::result::Result<WorkflowDefinition, String> {
        Ok((*self.definition).clone())
    }

    fn launch(
        &self,
        _execution_id: &str,
        pass: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    ) {
        let ended = Arc::clone(&self.ended);
        let handle = tokio::spawn(async move {
            pass.await;
            ended.notify_one();
        });
        *self.lock() = Some(handle);
    }
}

impl Session {
    /// Writes the owner's message to the conductor; its answer follows in
    /// the chat.
    pub(super) fn conductor_chat(&self, execution_id: &str, text: &str) -> Result<(), String> {
        let registry = self.conductor.registry()?;
        registry
            .post_chat(execution_id, text.trim())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Carries out a proposal of the conductor's, and drives the pass that
    /// carrying it out may start. A proposal some request already settled
    /// is refused, with how it was settled.
    pub(super) fn confirm_proposal(
        &mut self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<(), String> {
        let registry = Arc::clone(self.conductor.registry()?);
        let confirmed = registry
            .confirm_proposal(&self.definition, execution_id, proposal_id)
            .map_err(|error| error.to_string())?;
        if confirmed.outcome.repeated {
            return Err(already_settled(&confirmed.outcome.message));
        }
        if let Some(pass) = confirmed.pass {
            self.spawn_pass(pass);
        }
        Ok(())
    }

    /// Turns a proposal down.
    pub(super) fn reject_proposal(
        &self,
        execution_id: &str,
        proposal_id: &str,
    ) -> Result<(), String> {
        let outcome = self
            .conductor
            .registry()?
            .reject_proposal(execution_id, proposal_id)
            .map_err(|error| error.to_string())?;
        if outcome.repeated {
            return Err(already_settled(&outcome.message));
        }
        Ok(())
    }

    /// Takes back a chosen output of the conductor's. A pass driving the
    /// execution is stopped first, as the daemon's undo does.
    pub(super) async fn undo_decision(
        &self,
        execution_id: &str,
        decision_id: &str,
    ) -> Result<(), String> {
        let registry = Arc::clone(self.conductor.registry()?);
        let own_pass = self.pass.as_ref().map(JoinHandle::abort_handle);
        let passes = Arc::clone(self.conductor.passes());
        registry
            .undo_decision(&self.definition, execution_id, decision_id, move || {
                if let Some(pass) = own_pass {
                    pass.abort();
                }
                passes.abort();
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Runs the conductor on `harness` from its next invocation on, and
    /// remembers the choice on the storage.
    pub(super) fn set_conductor_harness(&self, harness: &str) -> Result<(), String> {
        self.conductor.registry()?;
        if let Some(reason) = harness_unavailable(harness) {
            return Err(reason);
        }
        self.storage
            .write_conductor_harness(harness)
            .map_err(|error| error.to_string())?;
        self.conductor.harness.set(harness);
        Ok(())
    }

    /// Uploads the conductor's state an owner waits for.
    pub(super) async fn serve_conductor_read(&mut self, request: ConductorReadRequest) {
        let (Some(client), Some(execution_id)) = (self.link.client(), self.execution_id.clone())
        else {
            return;
        };
        let read = ConductorView::read(
            self.engine.store(),
            &execution_id,
            request.after,
            request.limit as usize,
        );
        let view = match read {
            Ok(view) => match self.conductor.unavailable_reason() {
                None => view.available_on(self.conductor.harness.get()),
                Some(reason) => view.without_conductor(reason),
            },
            Err(error) => {
                tracing::warn!(%error, "cannot read the conductor's state");
                self.fail_read(&request.request_id, &error.to_string()).await;
                return;
            }
        };
        if let Err(error) = client
            .upload_conductor_read(&request.request_id, &view)
            .await
        {
            self.link.failed(&error, &self.timing);
        }
    }
}

fn already_settled(message: &str) -> String {
    format!("the proposal was already settled: {message}")
}
