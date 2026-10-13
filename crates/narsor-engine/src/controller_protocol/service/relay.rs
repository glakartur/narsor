//! The in-memory relay behind every read an owner asks of a live runner
//! (KTD4): conversation pages, execution events.
//!
//! A request waits here only while its owner waits; the runner picks it up
//! on its next poll and uploads the answer, which goes straight to that
//! owner and is kept nowhere. Each kind of read has its own request and
//! answer types over this one mechanism.

use std::collections::HashMap;
use std::sync::Mutex;

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

impl<Request, Answer> Relay<Request, Answer> {
    /// Forgets a request (answered, or its owner gave up).
    pub(super) fn close(&mut self, request_id: &str) {
        self.waiting.remove(request_id);
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

/// Closes a relay request when dropped, so an owner whose request future is
/// cancelled (a dropped connection, an aborted handler) before the answer or
/// the timeout does not leave its entry behind.
pub(super) struct RelayGuard<'a, Request, Answer> {
    relay: &'a Mutex<Relay<Request, Answer>>,
    request_id: String,
}

impl<'a, Request, Answer> RelayGuard<'a, Request, Answer> {
    pub(super) fn new(relay: &'a Mutex<Relay<Request, Answer>>, request_id: String) -> Self {
        Self { relay, request_id }
    }
}

impl<Request, Answer> Drop for RelayGuard<'_, Request, Answer> {
    fn drop(&mut self) {
        self.relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .close(&self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskId {
        TaskId::parse("task-1").expect("valid task id")
    }

    #[test]
    fn dropping_the_guard_closes_the_request() {
        let relay: Mutex<Relay<String, u8>> = Mutex::new(Relay::default());
        let (request_id, _answer) = relay.lock().unwrap().open("page", task(), |id| id);
        let guard = RelayGuard::new(&relay, request_id);
        assert_eq!(relay.lock().unwrap().requests_of(&task()).len(), 1);

        drop(guard);

        assert!(relay.lock().unwrap().requests_of(&task()).is_empty());
    }

    #[tokio::test]
    async fn an_aborted_waiting_future_leaves_no_request_behind() {
        let relay: std::sync::Arc<Mutex<Relay<String, u8>>> =
            std::sync::Arc::new(Mutex::new(Relay::default()));
        let waiter = {
            let relay = relay.clone();
            tokio::spawn(async move {
                let (request_id, answer) = relay.lock().unwrap().open("page", task(), |id| id);
                let _guard = RelayGuard::new(&relay, request_id);
                let _ = answer.await;
            })
        };
        tokio::task::yield_now().await;
        assert_eq!(relay.lock().unwrap().requests_of(&task()).len(), 1);

        waiter.abort();
        let _ = waiter.await;

        assert!(relay.lock().unwrap().requests_of(&task()).is_empty());
    }
}
