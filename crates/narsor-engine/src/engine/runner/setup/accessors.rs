//! What an [`Engine`] hands out about itself: its store, its sinks, its
//! event channel and the task labels its templates render.

use super::*;

impl Engine {
    pub(crate) fn reading_workspaces<'a>(&'a self, scope: TemplateScope<'a>) -> TemplateScope<'a> {
        match self.workspace_values.as_deref() {
            Some(values) => scope.with_workspace_values(values),
            None => scope,
        }
    }

    /// The ticket the operator typed, if any.
    pub fn task_ticket(&self) -> Option<&str> {
        self.task_labels.ticket.as_deref()
    }

    /// The title steps render as `task.title`: the operator's, else the
    /// generated one.
    pub fn task_title(&self) -> Option<String> {
        self.task_labels.effective_title()
    }

    /// The title the conductor generated, if it has.
    pub fn generated_title(&self) -> Option<String> {
        self.task_labels
            .generated_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Tells the engine the title the conductor generated, so the steps
    /// rendered from now on see it in `task.title`.
    pub fn set_generated_title(&self, title: Option<String>) {
        *self
            .task_labels
            .generated_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = title;
    }

    /// The store this engine persists to.
    pub fn store(&self) -> &ExecutionStore {
        &self.store
    }

    /// Where the tokens and cost that harnesses report go: the timeline
    /// file, announced to the reading notifier like an agent's.
    pub fn reading_sink(&self) -> Arc<dyn ReadingSink> {
        Arc::new(FileReadingSink::new(
            self.store.clone(),
            self.reading_notifier.clone(),
        ))
    }

    /// Subscribes to the live event stream.
    ///
    /// Every event the engine appends to a log is published here, for every
    /// execution it runs. This is the seam an SSE endpoint will take over
    /// (D38): the transport changes, the contract does not. A receiver that is
    /// dropped or falls behind never slows the engine down and never fails a
    /// step — the send result is deliberately ignored.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}
