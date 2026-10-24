//! The conductor names a task whose operator typed no title.
//!
//! The runner asks once, when the execution exists and no title is stored
//! ([`ConductorRegistry::request_title`]). The request runs beside the
//! execution like a gate's summary and never changes it. Its result is one
//! line kept in the conductor's store ([`ConductorStore::save_generated_title`]);
//! the runner reads it from there and reports it. A conductor that fails or
//! is interrupted leaves nothing: no chat entry, no problem, no retry.

use serde_json::json;

use crate::conductor::actor::InvocationOutcome;
use crate::conductor::response::{ConductorResponse, Decision};
use crate::conductor::store::{ConductorStore, QueuedRequest};
use crate::conductor::ConductorRegistry;
use crate::domain::{
    one_line_label, ConductorPurpose, ConductorTrigger, GENERATED_TITLE_MAX_CHARS,
};
use crate::persistence::Result;

impl ConductorRegistry {
    /// Queues the generation of the title of `execution_id` from the task
    /// `description`, once: nothing is queued when a title is stored or a
    /// request for one is waiting or running. Returns whether one was queued.
    ///
    /// The description reaches the conductor as data in the request, which
    /// its prompt tells it not to obey. Without a runtime to run it on, no
    /// title is made.
    pub fn request_title(&self, execution_id: &str, description: &str) -> Result<bool> {
        if tokio::runtime::Handle::try_current().is_err() || description.trim().is_empty() {
            return Ok(false);
        }
        let store = self.store(execution_id);
        if store.load_generated_title()?.is_some() {
            return Ok(false);
        }
        let state = store.load_state()?;
        let is_title = |request: &QueuedRequest| request.purpose == ConductorPurpose::GenerateTitle;
        if state.queue.iter().any(is_title)
            || state
                .pending
                .as_ref()
                .is_some_and(|pending| is_title(&pending.request))
        {
            return Ok(false);
        }
        let request = QueuedRequest::new(ConductorTrigger::Title, ConductorPurpose::GenerateTitle)
            .with_payload(json!({ "description": description }));
        self.actor(execution_id)?.submit(request)?;
        Ok(true)
    }
}

/// What a title request leaves: the title, when the conductor gave a usable
/// one; nothing otherwise.
pub(super) fn conclude_title(store: &ConductorStore, outcome: &InvocationOutcome) -> Result<()> {
    let InvocationOutcome::Answered(answer) = outcome else {
        return Ok(());
    };
    let Ok(ConductorResponse {
        decision: Decision::Title { title },
        ..
    }) = ConductorResponse::from_value(answer.response.clone(), ConductorPurpose::GenerateTitle)
    else {
        return Ok(());
    };
    match one_line_label(&title, GENERATED_TITLE_MAX_CHARS) {
        Some(title) => store.save_generated_title(&title),
        None => Ok(()),
    }
}
