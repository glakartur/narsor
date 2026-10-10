//! The task's conversation for owners: its per-attempt summary and one
//! attempt's pages — from the live runner (KTD4), or, for the daemon's own
//! local backend when its runner is not running, straight from the task's
//! storage, marked as such and with the time the record was last written.
//!
//! The same shape as the execution events (see [`super::events`]). A local
//! task without a live runner is never "unavailable right now": it has a
//! record (possibly without attempts), or its storage could not be read and
//! the answer says why. Only remote and archived tasks keep the old answer.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::events::{is_safe_component, LocalTaskStorage};
use super::relay::RelayGuard;
use super::{is_attempt_id, ProtocolService, DEFAULT_PAGE_BUDGET, MAX_PAGE_BUDGET};
use crate::controller_protocol::error::{ProtocolError, ProtocolResult};
use crate::controller_protocol::wire::*;
use crate::domain::TaskId;
use crate::engine::conversation_summary::{read_summaries, AttemptSummaries, ConversationCache};
use crate::orchestration::{
    ConversationAttemptSummary, ConversationAvailability, OwnerRef, TaskRecord,
};

/// The closed attempts' summaries per local task execution read from
/// storage — the daemon's own cache, apart from every runner's.
pub(super) type StoredSummaries = HashMap<(TaskId, String), ConversationCache>;

/// The code of a page the task's storage could not serve.
const RECORD_UNREADABLE: &str = "conversation_record_unreadable";

impl ProtocolService {
    /// The task's conversation summary, or why it is unavailable.
    ///
    /// A live runner's last report is the summary. Without one, the daemon's
    /// local backend reads the task's own storage (`source: storage`, with
    /// the record's age); any other backend, and an archived task, answers
    /// with the last safe summary and an explicit reason — never by
    /// recreating compute (KTD4).
    pub async fn conversation(
        &self,
        owner: &OwnerRef,
        task_id: &str,
    ) -> ProtocolResult<ConversationResponse> {
        let record = self.owned_task(owner, task_id)?;
        let now = self.control.now();
        let runner = record.runner.as_ref();
        let last_attempts = match runner.map(|runner| &runner.snapshot.conversation) {
            Some(ConversationAvailability::Live { attempts }) => attempts.clone(),
            _ => Vec::new(),
        };
        let observed_at = runner.map(|runner| runner.snapshot.observed_at);
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
            Some(ConversationUnavailableReason::Archived) => {
                self.forget_stored_conversations(&record.task_id);
                ConversationUnavailableReason::Archived
            }
            Some(reason) => match self.local_storage_of(&record) {
                Some(local) => {
                    return self
                        .stored_conversation(&record, local, now, last_attempts, observed_at)
                        .await;
                }
                None => reason,
            },
        };
        Ok(ConversationResponse::Unavailable {
            reason,
            last_attempts,
            observed_at,
            message: None,
        })
    }

    /// The summary from the task's storage: every attempt with a record,
    /// none when the runner has not named an execution yet or the storage
    /// holds none, or why the storage could not be read.
    async fn stored_conversation(
        &self,
        record: &TaskRecord,
        local: &LocalTaskStorage,
        now: DateTime<Utc>,
        last_attempts: Vec<ConversationAttemptSummary>,
        observed_at: Option<DateTime<Utc>>,
    ) -> ProtocolResult<ConversationResponse> {
        let unreadable = |message: String| ConversationResponse::Unavailable {
            reason: ConversationUnavailableReason::StorageUnreadable,
            last_attempts: last_attempts.clone(),
            observed_at,
            message: Some(message),
        };
        let Some(execution_id) = execution_id(record) else {
            return Ok(stored_summary(now, AttemptSummaries::default()));
        };
        // The id comes from a runner's report: it must never walk the path.
        if !is_safe_component(&execution_id) {
            return Ok(unreadable(
                "the execution id the runner reported is not valid".to_string(),
            ));
        }
        let key = (record.task_id.clone(), execution_id.clone());
        let mut cache = self
            .lock_stored_conversations()
            .remove(&key)
            .unwrap_or_default();
        let store = local.store_of(&record.task_id);
        let (read, cache) = tokio::task::spawn_blocking(move || {
            let read = if store.exists(&execution_id) {
                read_summaries(&store, &execution_id, &mut cache)
            } else {
                Ok(AttemptSummaries::default())
            };
            (read, cache)
        })
        .await
        .map_err(|error| ProtocolError::Internal(error.to_string()))?;
        self.lock_stored_conversations().insert(key, cache);
        Ok(match read {
            Ok(summaries) => stored_summary(now, summaries),
            Err(error) => unreadable(format!(
                "the conversation record could not be read: {error}"
            )),
        })
    }

    /// One bounded page of an attempt's raw conversation.
    ///
    /// From the live runner (KTD4): the request waits in memory until the
    /// runner's next poll picks it up and uploads the page, which is handed
    /// to this caller and kept nowhere — and gives up after
    /// [`super::ProtocolConfig::conversation_page_timeout`]. Without a live
    /// runner, the daemon's local backend reads the task's storage (the
    /// page says so, with the record's age); entry numbers are the same
    /// either way, so a reader continues from `next_entry` across the
    /// switch. Refused at once for any other backend and for an archived
    /// task.
    pub async fn conversation_page(
        &self,
        owner: &OwnerRef,
        task_id: &str,
        attempt_id: &str,
        query: ConversationPageQuery,
    ) -> ProtocolResult<ConversationPage> {
        let record = self.owned_task(owner, task_id)?;
        if !is_attempt_id(attempt_id) {
            return Err(invalid_attempt());
        }
        let blocker = match self.live_read_blocker(&record, self.control.now())? {
            None => return self.relayed_page(&record.task_id, attempt_id, query).await,
            Some(ConversationUnavailableReason::Archived) => {
                ConversationUnavailableReason::Archived
            }
            Some(reason) => match self.local_storage_of(&record) {
                Some(local) => return self.stored_page(&record, local, attempt_id, query).await,
                None => reason,
            },
        };
        Err(ProtocolError::conflict(
            "conversation_unavailable",
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
                "conversation_unavailable",
                "the runner could not read the conversation page",
            )),
            Err(_) => Err(ProtocolError::conflict(
                "conversation_unavailable",
                "the runner did not upload the conversation page in time",
            )),
        }
    }

    /// Reads the page from the task's storage, exactly as the runner would.
    async fn stored_page(
        &self,
        record: &TaskRecord,
        local: &LocalTaskStorage,
        attempt_id: &str,
        query: ConversationPageQuery,
    ) -> ProtocolResult<ConversationPage> {
        // Both ids name files: neither may walk the path.
        if !is_safe_component(attempt_id) {
            return Err(invalid_attempt());
        }
        let Some(execution_id) = execution_id(record) else {
            return Ok(ConversationPage::NoRecord);
        };
        if !is_safe_component(&execution_id) {
            return Err(ProtocolError::conflict(
                RECORD_UNREADABLE,
                "the execution id the runner reported is not valid",
            ));
        }
        let store = local.store_of(&record.task_id);
        let attempt_id = attempt_id.to_string();
        let budget = usize::try_from(page_budget(&query)).unwrap_or(usize::MAX);
        let read = tokio::task::spawn_blocking(move || {
            if !store.exists(&execution_id) {
                return Ok(None);
            }
            store
                .conversation_store(&execution_id)
                .read(&attempt_id, query.from, budget)
        })
        .await
        .map_err(|error| ProtocolError::Internal(error.to_string()))?
        .map_err(|error| {
            ProtocolError::conflict(
                RECORD_UNREADABLE,
                format!("the conversation record could not be read: {error}"),
            )
        })?;
        let recorded_at = read.as_ref().and_then(|snapshot| snapshot.recorded_at);
        Ok(with_source(
            ConversationPage::from_snapshot(read),
            ExecutionEventsSource::Storage,
            recorded_at,
        ))
    }

    /// The storage this host holds for the task, when its backend is the
    /// daemon's own.
    fn local_storage_of(&self, record: &TaskRecord) -> Option<&LocalTaskStorage> {
        self.local_storage
            .as_ref()
            .filter(|local| local.backend_id == record.assignment.backend_id)
    }

    /// Drops what was cached of an archived task: its storage is gone.
    fn forget_stored_conversations(&self, task_id: &TaskId) {
        self.lock_stored_conversations()
            .retain(|(task, _), _| task != task_id);
    }

    fn lock_stored_conversations(&self) -> std::sync::MutexGuard<'_, StoredSummaries> {
        self.stored_conversations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The execution the runner last named.
fn execution_id(record: &TaskRecord) -> Option<String> {
    record
        .runner
        .as_ref()
        .and_then(|runner| runner.snapshot.execution_cursor.execution_id.clone())
}

fn stored_summary(now: DateTime<Utc>, summaries: AttemptSummaries) -> ConversationResponse {
    ConversationResponse::Available {
        attempts: summaries.attempts,
        observed_at: now,
        source: ExecutionEventsSource::Storage,
        recorded_at: summaries.recorded_at,
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
