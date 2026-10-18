//! The task's conversation for owners: its per-attempt summary and one
//! attempt's pages — from the runner, which is the task's own while it runs
//! and a read-only runner while the task is stopped (D63). The control plane
//! never reads task storage itself.
//!
//! A stopped task's summary is the last one its runner reported (and says
//! why it is not live); the pages of its attempts come from the reader.

use chrono::{DateTime, Utc};

use super::reader::ReaderReady;
use super::relay::RelayGuard;
use super::{is_attempt_id, ProtocolService, ReadBlocker, DEFAULT_PAGE_BUDGET, MAX_PAGE_BUDGET};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::orchestration::{ConversationAttemptSummary, ConversationAvailability, OwnerRef};

impl ProtocolService {
    /// The task's conversation summary, or why it is unavailable.
    ///
    /// A live runner's last report is the summary. Without one — the task
    /// is stopped, interrupted, archived — the answer says why, with the
    /// last safe summary the runner reported (R12). It does not start a
    /// read-only runner: its pages, which a client reads next, do.
    pub async fn conversation(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<ConversationResponse> {
        let record = self.owned_task(owner, task_id)?;
        self.note_details_read(&record);
        let now = self.control.now();
        let runner = record.runner.as_ref();
        let last_attempts: Vec<ConversationAttemptSummary> =
            match runner.map(|runner| &runner.snapshot.conversation) {
                Some(ConversationAvailability::Live { attempts }) => attempts.clone(),
                _ => Vec::new(),
            };
        let observed_at: Option<DateTime<Utc>> = runner.map(|runner| runner.snapshot.observed_at);
        let reason = match self.live_read_blocker(&record, now)? {
            None => {
                if let Some(observed_at) = observed_at {
                    return Ok(ConversationResponse::Available {
                        attempts: last_attempts,
                        observed_at,
                        source: ExecutionEventsSource::Runner,
                        recorded_at: None,
                    });
                }
                ConversationUnavailableReason::NotStarted
            }
            Some(ReadBlocker::Unavailable(reason) | ReadBlocker::Reader(reason)) => reason,
        };
        Ok(ConversationResponse::Unavailable {
            reason,
            last_attempts,
            observed_at,
            message: None,
        })
    }

    /// One bounded page of an attempt's raw conversation.
    ///
    /// From the runner (KTD4): the request waits in memory until the
    /// runner's next poll picks it up and uploads the page, which is handed
    /// to this caller and kept nowhere — and gives up after
    /// [`super::ProtocolConfig::conversation_page_timeout`]. A stopped task
    /// is served by a read-only runner the control plane has the controller
    /// start (D63); until it is connected the request is refused with
    /// [`CONVERSATION_READER_STARTING`] and the client asks again. Entry
    /// numbers are the same whichever runner answers, so a reader continues
    /// from `next_entry`. Refused at once for any state nobody can serve.
    pub async fn conversation_page(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        attempt_id: &str,
        query: ConversationPageQuery,
    ) -> ProtocolResult<ConversationPage> {
        let record = self.owned_task(owner, task_id)?;
        self.note_details_read(&record);
        if !is_attempt_id(attempt_id) {
            return Err(invalid_attempt());
        }
        let blocker = match self.live_read_blocker(&record, self.control.now())? {
            None => return self.relayed_page(&record.task_id, attempt_id, query).await,
            Some(ReadBlocker::Unavailable(reason)) => reason,
            Some(ReadBlocker::Reader(reason)) => match self.reader_ready(&record).await? {
                ReaderReady::Ready => {
                    return self.relayed_page(&record.task_id, attempt_id, query).await
                }
                ReaderReady::Starting => return Err(ProtocolError::conflict(
                    CONVERSATION_READER_STARTING,
                    "the task is stopped and its read-only runner is starting; ask again shortly",
                )),
                ReaderReady::NotReadable => reason,
            },
        };
        Err(ProtocolError::conflict(
            CONVERSATION_UNAVAILABLE,
            format!("the task's conversation cannot be read right now ({blocker:?})"),
        ))
    }

    /// Asks the live runner and waits for its upload.
    async fn relayed_page(
        &self,
        task_id: &TaskId,
        attempt_id: &str,
        query: ConversationPageQuery,
    ) -> ProtocolResult<ConversationPage> {
        let (request_id, answer) = self
            .lock_pages()
            .open("page", task_id.clone(), |request_id| {
                ConversationPageRequest {
                    request_id,
                    attempt_id: attempt_id.to_string(),
                    from_entry: query.from,
                    byte_budget: page_budget(&query),
                }
            });
        // Wakes the runner's long-poll, which then sees the request.
        let guard = RelayGuard::new(&self.pages, request_id);
        self.notify(task_id);
        let page = tokio::time::timeout(self.config.conversation_page_timeout, answer).await;
        drop(guard);
        match page {
            Ok(Ok(page)) => Ok(with_source(page, ExecutionEventsSource::Runner, None)),
            Ok(Err(_)) => Err(ProtocolError::conflict(
                CONVERSATION_UNAVAILABLE,
                "the runner could not read the conversation page",
            )),
            Err(_) => Err(ProtocolError::conflict(
                CONVERSATION_UNAVAILABLE,
                "the runner did not upload the conversation page in time",
            )),
        }
    }
}

/// The raw bytes a page may hold: the owner's ask, bounded.
fn page_budget(query: &ConversationPageQuery) -> u64 {
    query
        .byte_budget
        .unwrap_or(DEFAULT_PAGE_BUDGET)
        .clamp(1, MAX_PAGE_BUDGET)
}

/// Marks a page with where it was read from.
fn with_source(
    page: ConversationPage,
    read_from: ExecutionEventsSource,
    when: Option<DateTime<Utc>>,
) -> ConversationPage {
    match page {
        ConversationPage::Recorded {
            entries,
            next_entry,
            closed,
            truncated_tail,
            ..
        } => ConversationPage::Recorded {
            entries,
            next_entry,
            closed,
            truncated_tail,
            source: Some(read_from),
            recorded_at: when,
        },
        ConversationPage::NoRecord => ConversationPage::NoRecord,
    }
}

fn invalid_attempt() -> ProtocolError {
    ProtocolError::bad_request(
        "invalid_attempt",
        "an attempt id is 1-128 characters of [A-Za-z0-9._-], not starting with '.'",
    )
}
