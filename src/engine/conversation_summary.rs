//! Per-attempt summaries of an execution's conversation records (KTD4).
//!
//! Counts and times only, never content. Built the same way wherever the
//! storage is read — by the runner for its snapshots, and by the daemon for
//! a local task whose runner is not running — so both say the same thing
//! about the same storage.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::{build_timeline, Timeline};
use crate::orchestration::ConversationAttemptSummary;
use crate::persistence::{ConversationStore, ExecutionStore, PersistenceError};

/// Summaries of attempts whose conversation is closed: they never change
/// again, so each is counted once per cache. One cache serves one
/// execution; every reader keeps its own.
#[derive(Debug, Default)]
pub struct ConversationCache {
    closed: HashMap<String, ConversationAttemptSummary>,
}

/// The attempts of one execution that have a conversation record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptSummaries {
    /// One summary per attempt, in timeline order.
    pub attempts: Vec<ConversationAttemptSummary>,
    /// When the newest of the records was last written (see
    /// [`crate::persistence::ConversationSnapshot::recorded_at`]); `None`
    /// when no record holds an entry.
    pub recorded_at: Option<DateTime<Utc>>,
}

/// Reads the summaries of `execution_id` on `store`: the timeline from its
/// event log, then every attempt's record.
pub fn read_summaries(
    store: &ExecutionStore,
    execution_id: &str,
    cache: &mut ConversationCache,
) -> Result<AttemptSummaries, PersistenceError> {
    let execution = store.load_execution(execution_id)?;
    let events = store.event_log(execution_id).read_all()?;
    let conversations = store.conversation_store(execution_id);
    let recorded = conversations.recorded_attempt_ids()?;
    let timeline = build_timeline(&events, execution.status, &recorded);
    summaries(&conversations, &recorded, &timeline, cache)
}

/// One summary per attempt with a conversation record, in timeline order.
pub fn summaries(
    conversations: &ConversationStore,
    recorded: &HashSet<String>,
    timeline: &Timeline,
    cache: &mut ConversationCache,
) -> Result<AttemptSummaries, PersistenceError> {
    let state_of: HashMap<&str, &str> = timeline
        .positions
        .iter()
        .filter_map(|position| {
            position
                .action_execution_id
                .as_deref()
                .map(|attempt| (attempt, position.state_id.as_str()))
        })
        .collect();
    let mut ordered: Vec<&String> = recorded.iter().collect();
    let order: HashMap<&str, usize> = timeline
        .positions
        .iter()
        .enumerate()
        .filter_map(|(index, position)| {
            position
                .action_execution_id
                .as_deref()
                .map(|attempt| (attempt, index))
        })
        .collect();
    ordered.sort_by_key(|attempt| (order.get(attempt.as_str()).copied(), (*attempt).clone()));

    let mut summaries = AttemptSummaries {
        attempts: Vec::with_capacity(ordered.len()),
        recorded_at: None,
    };
    for attempt in ordered {
        if let Some(summary) = cache.closed.get(attempt) {
            // A closed record was last written with its last entry.
            summaries.recorded_at = summaries.recorded_at.max(summary.last_entry_at);
            summaries.attempts.push(summary.clone());
            continue;
        }
        let Some(read) = conversations.read(attempt, 0, usize::MAX)? else {
            continue;
        };
        let summary = ConversationAttemptSummary {
            attempt_id: attempt.clone(),
            state_id: state_of
                .get(attempt.as_str())
                .map(|state| state.to_string()),
            entries: read.entries.len() as u64,
            closed: read.closed,
            last_entry_at: read.entries.last().map(|entry| entry.timestamp),
        };
        summaries.recorded_at = summaries.recorded_at.max(read.recorded_at);
        if summary.closed {
            cache.closed.insert(attempt.clone(), summary.clone());
        }
        summaries.attempts.push(summary);
    }
    Ok(summaries)
}
