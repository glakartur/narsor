use super::*;
use crate::engine::timeline_file::{reading_from_element, ReadingSink};
use narsor_protocol::chat::TechnicalCategory;

/// Receives the agent-session events of a run.
///
/// The executor is shared between executions, so the execution the events
/// belong to is passed in per call rather than held.
pub trait AgentEventSink: Send + Sync + std::fmt::Debug {
    /// Records one event of `execution_id`.
    ///
    /// Recording must never fail a run: an event that cannot be written is
    /// logged and dropped, the same way the engine's live channel ignores a
    /// receiver that went away.
    fn emit(&self, execution_id: &str, data: EventData);
}

/// Receives the usage limits a harness reported during a run (R8).
///
/// Shared between executions, like [`AgentEventSink`]. The runner installs
/// its usage forwarder, which buffers and sends the reports; without one
/// (the daemon's engine, a direct unit test) the limits are dropped.
pub trait UsageReporter: Send + Sync + std::fmt::Debug {
    /// Takes the latest usage limits of one harness, marked with its id in
    /// `report.harness`.
    ///
    /// Reporting must never block or fail a run: the report is handed on
    /// and the call returns, whatever becomes of it later.
    fn report(&self, report: HarnessUsageReport);
}

/// Where the usage limits read from one harness's stdout go: the reporter,
/// with the harness they belong to. It only passes each recognized report
/// on, marked with the harness id: it holds no state, and the buffering and
/// sending are the reporter's business.
#[derive(Clone, Copy)]
pub(crate) struct UsageRelay<'a> {
    pub(super) harness: &'a str,
    pub(super) reporter: &'a dyn UsageReporter,
}

impl UsageRelay<'_> {
    /// Reports `limits`, observed now.
    fn relay(&self, limits: Vec<UsageLimit>) {
        self.reporter.report(HarnessUsageReport {
            harness: self.harness.to_string(),
            observed_at: Utc::now(),
            limits,
        });
    }
}

/// Records one stdout line the parser has just been fed, with its
/// `elements`: in the conversation, unless the line was a usage report.
///
/// A usage report is not part of the conversation (R8a): its limits go to
/// `usage` instead, when there are any and somewhere to send them.
pub(crate) fn record_stdout_line(
    parser: &mut dyn HarnessStreamParser,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
    line: &[u8],
    elements: &[ChatElement],
) {
    match parser.take_usage() {
        None => conversation.append(ConversationSource::Stdout, line, elements),
        Some(limits) if limits.is_empty() => {}
        Some(limits) => {
            if let Some(usage) = usage {
                usage.relay(limits);
            }
        }
    }
}

/// The sink the engine installs: append to the execution's log, then publish.
#[derive(Debug, Clone)]
pub struct LoggingEventSink {
    store: ExecutionStore,
    events: Option<broadcast::Sender<Event>>,
}

impl LoggingEventSink {
    /// A sink writing to `store` and publishing on `events`.
    pub fn new(store: ExecutionStore, events: Option<broadcast::Sender<Event>>) -> Self {
        Self { store, events }
    }
}

impl AgentEventSink for LoggingEventSink {
    fn emit(&self, execution_id: &str, data: EventData) {
        match self
            .store
            .event_log(execution_id)
            .append_next(&execution_id.to_string(), data)
        {
            Ok(event) => {
                if let Some(events) = &self.events {
                    let _ = events.send(event);
                }
            }
            Err(error) => {
                tracing::warn!(
                    execution_id,
                    %error,
                    "an agent session event could not be appended to the log"
                );
            }
        }
    }
}

/// Best-effort access to one attempt's conversation file (U3).
///
/// Wraps the writer in `Arc<Mutex<_>>` because stdout and stderr are drained
/// concurrently (see [`stream_lines`]) and both append to the same file; the
/// lock is held only for the duration of one `write_all`, never across an
/// `.await`. `None` stands for "there is nowhere to write" — no store was
/// given, or opening the file failed — and every method is then a no-op,
/// following the same "recording must never fail a run" rule as
/// [`AgentEventSink`].
#[derive(Clone)]
pub(crate) struct ConversationSink(Option<Arc<Mutex<ConversationWriter>>>, Option<ReadingTap>);

/// Where the use a harness reports in its output is handed on, and what it
/// is a reading of.
#[derive(Clone)]
pub(super) struct ReadingTap {
    pub(super) sink: Arc<dyn ReadingSink>,
    pub(super) execution_id: String,
    pub(super) attempt_id: String,
    pub(super) harness: String,
}

/// The elements as the conversation file keeps them. What an agent used lives
/// in the timeline file alone, so a result loses its tokens and cost; one
/// left with nothing to say (its text repeats the reply, and it is no
/// error) is not kept.
fn without_usage(elements: &[ChatElement]) -> Vec<ChatElement> {
    elements
        .iter()
        .filter_map(|element| match element {
            ChatElement::RunResult {
                text,
                structured_output,
                is_error,
                ..
            } if !*is_error
                && structured_output.is_none()
                && text.as_deref().is_none_or(|text| text.trim().is_empty()) =>
            {
                None
            }
            ChatElement::RunResult { .. } => {
                let mut element = element.clone();
                if let ChatElement::RunResult {
                    cost_usd,
                    usage,
                    subagent_stats,
                    ..
                } = &mut element
                {
                    *cost_usd = None;
                    *subagent_stats = None;
                    *usage = None;
                }
                Some(element)
            }
            other => Some(other.clone()),
        })
        .collect()
}

impl ConversationSink {
    /// No file to write to.
    pub(crate) fn none() -> Self {
        Self(None, None)
    }

    /// Opens `attempt_id`'s conversation file in `store`, or answers `none()`
    /// and logs why when that fails.
    pub(super) fn open(store: &ConversationStore, attempt_id: &str) -> Self {
        match store.writer(attempt_id) {
            Ok(writer) => Self(Some(Arc::new(Mutex::new(writer))), None),
            Err(error) => {
                tracing::warn!(
                    attempt_id,
                    %error,
                    "could not open this attempt's conversation file; the run \
                     continues without one"
                );
                Self::none()
            }
        }
    }

    /// The sink handing the use that the harness reports on to `tap`.
    pub(super) fn with_readings(mut self, tap: ReadingTap) -> Self {
        self.1 = Some(tap);
        self
    }

    /// Appends one entry with its elements, logging (and otherwise
    /// ignoring) a failure to.
    pub(super) fn append(&self, source: ConversationSource, raw: &[u8], elements: &[ChatElement]) {
        let already_empty = elements.is_empty();
        if source == ConversationSource::Stdout {
            if let Some(tap) = &self.1 {
                for element in elements {
                    if let Some(reading) = reading_from_element(element, Some(&tap.harness)) {
                        tap.sink.record(&tap.execution_id, &tap.attempt_id, reading);
                    }
                }
            }
        }
        let Some(writer) = &self.0 else { return };
        let elements = without_usage(elements);
        if elements.is_empty() && !already_empty {
            return;
        }
        let mut writer = writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = writer.append(source, raw, &elements) {
            tracing::warn!(%error, "could not append a conversation entry");
        }
    }

    /// Convenience for a narsor-authored entry (the prompt, the outcome).
    pub(super) fn append_narsor(&self, text: &str, elements: &[ChatElement]) {
        self.append(ConversationSource::Narsor, text.as_bytes(), elements);
    }

    /// A narsor-authored entry about the harness process — its exit code,
    /// timeout, or failure to run — whose one element is a
    /// [`TechnicalCategory::Process`] event summarized by the same `text`.
    pub(super) fn append_outcome(&self, text: &str, data: Value) {
        let element = ChatElement::Technical {
            category: TechnicalCategory::Process,
            summary: text.to_string(),
            data: Some(data),
        };
        self.append_narsor(text, &[element]);
    }

    /// Flushes the writer to disk, once, at the very end of the attempt.
    ///
    /// Consumes the sink: nothing appends after an attempt has finished.
    /// Silently does nothing if another clone of this sink is still alive
    /// when this is called — which never happens in practice, since every
    /// reader task has already joined by the time [`AgentExecutor::run`]
    /// calls this.
    pub(super) fn finish(self) {
        let Some(writer) = self.0 else { return };
        match Arc::try_unwrap(writer) {
            Ok(mutex) => {
                let writer = mutex
                    .into_inner()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = writer.finish() {
                    tracing::warn!(%error, "could not sync this attempt's conversation file");
                }
            }
            Err(_still_shared) => {
                tracing::warn!(
                    "the conversation writer still had outstanding references at the \
                     end of the attempt; it was not synced explicitly"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(text: Option<&str>, is_error: bool) -> ChatElement {
        ChatElement::RunResult {
            text: text.map(str::to_string),
            structured_output: None,
            stop_reason: None,
            is_error,
            cost_usd: Some(0.5),
            usage: Some(serde_json::json!({ "input_tokens": 1 })),
            subagent_stats: None,
            turns: Some(2),
            duration_ms: Some(3),
        }
    }

    #[test]
    fn a_result_with_nothing_to_say_is_not_kept() {
        assert!(without_usage(&[result(None, false)]).is_empty());
    }

    #[test]
    fn a_kept_result_has_no_tokens_or_cost() {
        for element in [result(Some("answer"), false), result(None, true)] {
            match without_usage(&[element]).as_slice() {
                [ChatElement::RunResult {
                    cost_usd, usage, ..
                }] => assert!(cost_usd.is_none() && usage.is_none()),
                other => panic!("expected the result, got {other:?}"),
            }
        }
    }
}
