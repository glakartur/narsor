//! How an [`Engine`] is built: its constructor, the `with_*` builders, and
//! the conductor attached after it.

use super::*;

/// The agent executor this engine runs, wired to its own store and channel.
fn agent_executor(
    store: &ExecutionStore,
    events: &broadcast::Sender<Event>,
    on_reading: Option<Arc<dyn Fn() + Send + Sync>>,
) -> AgentExecutor {
    AgentExecutor::new()
        .with_event_sink(Arc::new(LoggingEventSink::new(
            store.clone(),
            Some(events.clone()),
        )))
        .with_reading_sink(Arc::new(FileReadingSink::new(store.clone(), on_reading)))
}

impl Engine {
    /// Creates an engine with the executors of this version: `script`, `human`
    /// and `agent`.
    ///
    /// `wait` is deliberately unregistered — it is Phase 3 — so a workflow that
    /// uses one fails with a clear message instead of behaving like something
    /// it is not. [`Engine::with_executor`] fills it in.
    ///
    /// The agent executor is given a [`LoggingEventSink`] over this engine's
    /// own store and live channel, so the agent-session events it emits land in
    /// the same log, in the same order, as the engine's own.
    pub fn new(store: ExecutionStore) -> Self {
        let events = broadcast::channel(EVENT_CHANNEL_CAPACITY).0;

        let agent = agent_executor(&store, &events, None);

        let mut executors: HashMap<ActionKind, Arc<dyn ActionExecutor>> = HashMap::new();
        executors.insert(ActionKind::Script, Arc::new(ScriptExecutor::new()));
        executors.insert(ActionKind::Human, Arc::new(HumanExecutor::new()));
        executors.insert(ActionKind::Profile, Arc::new(ProfileExecutor::like(&agent)));
        executors.insert(ActionKind::Agent, Arc::new(agent));

        Self {
            store,
            executors,
            agent_timeout_secs: None,
            agent_args: AgentArgs::new(),
            escalation_threshold: DEFAULT_ESCALATION_THRESHOLD,
            usage_reporter: None,
            reading_notifier: None,
            default_harness: Arc::new(BUILT_IN_HARNESS.to_string()),
            events,
            pause_requests: Mutex::new(HashSet::new()),
            active_runs: Mutex::new(HashSet::new()),
            checkpoints: false,
            conductor: OnceLock::new(),
            task_labels: TaskLabels::default(),
            workspace_values: None,
        }
    }

    /// Returns the engine rendering `task.ticket` and `task.title` from the
    /// operator's values in the task's launch spec.
    pub fn with_task_labels(mut self, ticket: Option<String>, title: Option<String>) -> Self {
        self.task_labels.ticket = ticket;
        self.task_labels.title = title;
        self
    }

    /// Returns the engine reading the workspace values the runner owns
    /// (`context.workspace.NAME.FIELD`) from `values`. Without them those
    /// fields render as empty text.
    pub fn with_workspace_values(mut self, values: Arc<dyn WorkspaceValues>) -> Self {
        self.workspace_values = Some(values);
        self
    }

    /// Returns the engine with the configured defaults for `agent` actions:
    /// a fixed default harness and the time limit.
    pub fn with_agent_defaults(self, defaults: AgentDefaults) -> Self {
        self.with_agent_timeout(defaults.timeout_secs)
            .with_agent_args(defaults.args)
            .with_escalation_threshold(defaults.escalation_threshold)
            .with_default_harness_source(Arc::new(defaults.harness))
    }

    /// Returns the engine raising a step's model or effort after `threshold`
    /// consecutive failed attempts; `0` turns escalation off.
    pub fn with_escalation_threshold(mut self, threshold: u32) -> Self {
        self.escalation_threshold = threshold;
        self
    }

    /// Returns the engine adding `args` to every harness run of its agent
    /// steps; on a conflict they replace the arguments Narsor chose.
    ///
    /// Rebuilds the stock agent executor, like [`Self::with_usage_reporter`].
    pub fn with_agent_args(mut self, args: AgentArgs) -> Self {
        self.agent_args = args;
        self.rebuild_agent_executor();
        self
    }

    fn rebuild_agent_executor(&mut self) {
        let mut agent = agent_executor(&self.store, &self.events, self.reading_notifier.clone())
            .with_extra_args(self.agent_args.clone());
        if let Some(reporter) = &self.usage_reporter {
            agent = agent.with_usage_reporter(Arc::clone(reporter));
        }
        self.executors
            .insert(ActionKind::Profile, Arc::new(ProfileExecutor::like(&agent)));
        self.executors.insert(ActionKind::Agent, Arc::new(agent));
    }

    /// Returns the engine with the time limit of an `agent` action that
    /// declares none, `None` for no limit.
    pub fn with_agent_timeout(mut self, timeout_secs: Option<u64>) -> Self {
        self.agent_timeout_secs = timeout_secs;
        self
    }

    /// Returns the engine with `source` answering which harness an `agent`
    /// action that names none runs on — read once per action, at its first
    /// attempt (R16a). Whatever executor is registered for `agent` actions
    /// is handed the answer.
    pub fn with_default_harness_source(mut self, source: Arc<dyn DefaultHarnessSource>) -> Self {
        self.default_harness = source;
        self
    }

    /// Writes a checkpoint of the workspace at every step boundary (R4).
    ///
    /// Opt-in while there is no hub to replicate to: turning it on for every
    /// engine would make each step of every existing test shell out to `git`
    /// for no gain. From M2 on, the hub's worker turns it on unconditionally.
    pub fn with_checkpoints(mut self) -> Self {
        self.checkpoints = true;
        self
    }

    /// Returns the engine with the usage limits its agent steps' harnesses
    /// report routed to `reporter` (R8).
    ///
    /// Rebuilds the stock agent executor with the reporter, so it replaces
    /// an executor registered for `agent` actions before; one registered
    /// after is the caller's to give the reporter to
    /// ([`AgentExecutor::with_usage_reporter`]).
    pub fn with_usage_reporter(mut self, reporter: Arc<dyn UsageReporter>) -> Self {
        self.usage_reporter = Some(reporter);
        self.rebuild_agent_executor();
        self
    }

    /// Returns the engine calling `notify` whenever the tokens or cost its
    /// agents report have been written to the execution's timeline file.
    ///
    /// Rebuilds the stock agent executor, like [`Self::with_usage_reporter`].
    pub fn with_reading_notifier(mut self, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.reading_notifier = Some(notify);
        self.rebuild_agent_executor();
        self
    }

    /// Registers (or replaces) the executor for one action type.
    pub fn with_executor(mut self, kind: ActionKind, executor: Arc<dyn ActionExecutor>) -> Self {
        self.executors.insert(kind, executor);
        self
    }

    /// Hands every step whose agent gives no usable output label, even
    /// when asked again, to `registry`'s conductor (F1, U10).
    ///
    /// Held weakly: the registry holds the engine, and the engine must not
    /// keep it alive in return. Whoever attaches it keeps it — the daemon,
    /// for as long as it runs. Without one (or once it is gone) such a step
    /// becomes a problem for the operator straight away, as it did before
    /// the conductor existed. Only the first registry attached counts.
    pub fn attach_conductor(&self, registry: &Arc<ConductorRegistry>) {
        if self.conductor.set(Arc::downgrade(registry)).is_err() {
            tracing::warn!("a conductor is already attached to this engine; ignoring another");
        }
    }

    /// The conductor attached to this engine, while it is alive.
    pub(crate) fn conductor(&self) -> Option<Arc<ConductorRegistry>> {
        self.conductor.get().and_then(Weak::upgrade)
    }
}
