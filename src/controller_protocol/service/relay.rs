//! The in-memory relay behind every read an owner asks of a live runner
//! (KTD4): conversation pages, execution events.
//!
//! A request waits here only while its owner waits; the runner picks it up
//! on its next poll and uploads the answer, which goes straight to that
//! owner and is kept nowhere. Each kind of read has its own request and
//! answer types over this one mechanism.

use std::collections::HashMap;

use tokio::sync::oneshot;

use crate::domain::TaskId;

/// Requests of one kind that owners are waiting on.
pub(super) struct Relay<Request, Answer> {
    next_id: u64,
    waiting: HashMap<String, Waiting<Request, Answer>>,
}

struct Waiting<Request, Answer> {
    task_id: TaskId,
    request: Request,
    reply: oneshot::Sender<Answer>,
}

impl<Request, Answer> Default for Relay<Request, Answer> {
    fn default() -> Self {
        Self {
            next_id: 0,
            waiting: HashMap::new(),
        }
    }
}

impl<Request: Clone, Answer> Relay<Request, Answer> {
    /// Registers a request of `task_id`; `build` receives its relay id
    /// (`<prefix>-<n>`). Returns the id and the channel the answer comes on.
    pub(super) fn open(
        &mut self,
        prefix: &str,
        task_id: TaskId,
        build: impl FnOnce(String) -> Request,
    ) -> (String, oneshot::Receiver<Answer>) {
        self.next_id += 1;
        let request_id = format!("{prefix}-{}", self.next_id);
        let (reply, answer) = oneshot::channel();
        self.waiting.insert(
            request_id.clone(),
            Waiting {
                task_id,
                request: build(request_id.clone()),
                reply,
            },
        );
        (request_id, answer)
    }

    /// Forgets a request (answered, or its owner gave up).
    pub(super) fn close(&mut self, request_id: &str) {
        self.waiting.remove(request_id);
    }

    /// Takes the request `request_id` of `task_id` — only the task's own
    /// runner may answer it — returning it with the owner's channel.
    pub(super) fn take(
        &mut self,
        request_id: &str,
        task_id: &TaskId,
    ) -> Option<(Request, oneshot::Sender<Answer>)> {
        match self.waiting.get(request_id) {
            Some(waiting) if &waiting.task_id == task_id => self
                .waiting
                .remove(request_id)
                .map(|waiting| (waiting.request, waiting.reply)),
            _ => None,
        }
    }

    /// The requests of `task_id` owners wait on right now, by relay id.
    pub(super) fn requests_of(&self, task_id: &TaskId) -> Vec<Request> {
        let mut found: Vec<_> = self
            .waiting
            .iter()
            .filter(|(_, waiting)| &waiting.task_id == task_id)
            .map(|(id, waiting)| (id, waiting.request.clone()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(b.0));
        found.into_iter().map(|(_, request)| request).collect()
    }
}
