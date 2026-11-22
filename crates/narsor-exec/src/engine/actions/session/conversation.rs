use super::*;
use crate::engine::timeline_file::{reading_from_element, ReadingSink, ReportedSession};
use narsor_protocol::chat::{SubagentRef, TechnicalCategory};

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
/// `elements` and the `subagent` that wrote it (`None` for the main agent):
/// in the conversation, unless the line was a usage report. A harness
/// session id the stream has said by now goes to the conversation's
/// reading tap, so it is kept before the attempt ends.
///
/// A usage report is not part of the conversation (R8a): its limits go to
/// `usage` instead, when there are any and somewhere to send them.
pub(crate) fn record_stdout_line(
    parser: &mut dyn HarnessStreamParser,
    conversation: &ConversationSink,
    usage: Option<UsageRelay<'_>>,
    line: &[u8],
    elements: &[ChatElement],
    subagent: Option<&SubagentRef>,
) {
    if let Some(session_id) = parser.session_id() {
        conversation.report_session(session_id);
    }
    match parser.take_usage() {
        None => conversation.append(ConversationSource::Stdout, line, elements, subagent),
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
pub(crate) struct ReadingTap {
    sink: Arc<dyn ReadingSink>,
    execution_id: String,
    attempt_id: String,
    harness: String,
    /// The harness session id last handed on, so a harness that repeats it
    /// on every line has it written once.
    reported_session: Arc<Mutex<Option<String>>>,
}

impl ReadingTap {
    pub(crate) fn new(
        sink: Arc<dyn ReadingSink>,
        execution_id: String,
        attempt_id: String,
        harness: String,
    ) -> Self {
        Self {
            sink,
            execution_id,
            attempt_id,
            harness,
            reported_session: Arc::new(Mutex::new(None)),
        }
    }

    /// Hands `session_id` on when it is not the one handed on last.
    fn report_session(&self, session_id: &str) {
        let mut reported = self
            .reported_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reported.as_deref() == Some(session_id) {
            return;
        }
        *reported = Some(session_id.to_string());
        self.sink.record_session(
            &self.execution_id,
            &self.attempt_id,
            ReportedSession {
                harness: self.harness.clone(),
                harness_session_id: session_id.to_string(),
            },
        );
    }
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

    /// Hands the harness session id the stream has said on to the reading
    /// tap, when there is one.
    fn report_session(&self, session_id: &str) {
        if let Some(tap) = &self.1 {
            tap.report_session(session_id);
        }
    }

    /// Appends one entry with its elements, logging (and otherwise
    /// ignoring) a failure to.
    pub(super) fn append(
        &self,
        source: ConversationSource,
        raw: &[u8],
        elements: &[ChatElement],
        subagent: Option<&SubagentRef>,
    ) {
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
        if let Err(error) = writer.append_for_subagent(source, raw, &elements, subagent) {
            tracing::warn!(%error, "could not append a conversation entry");
        }
    }

    /// Convenience for a narsor-authored entry (the prompt, the outcome).
    pub(crate) fn append_narsor(&self, text: &str, elements: &[ChatElement]) {
        self.append(ConversationSource::Narsor, text.as_bytes(), elements, None);
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
pub(crate) mod tests {
    use super::*;
    use crate::engine::actions::answer::AnswerShape;
    use crate::engine::actions::harness::{
        ClaudeCodeAdapter, CodexAdapter, GitHubCopilotAdapter, HarnessAdapter, OpenCodeAdapter,
    };
    use crate::engine::timeline_file::UsageReading;

    /// A reading sink that keeps the sessions it is given.
    #[derive(Debug, Default)]
    pub(crate) struct RecordedSessions(pub(crate) Mutex<Vec<(String, ReportedSession)>>);

    impl ReadingSink for RecordedSessions {
        fn record(&self, _execution_id: &str, _attempt_id: &str, _reading: UsageReading) {}

        fn record_session(&self, _execution_id: &str, attempt_id: &str, session: ReportedSession) {
            self.0
                .lock()
                .unwrap()
                .push((attempt_id.to_string(), session));
        }
    }

    /// A conversation sink handing what the harness reports to `sessions`,
    /// for attempt `a1` on `harness`.
    pub(crate) fn tapped(sessions: &Arc<RecordedSessions>, harness: &str) -> ConversationSink {
        ConversationSink::none().with_readings(ReadingTap::new(
            Arc::clone(sessions) as Arc<dyn ReadingSink>,
            "e1".to_string(),
            "a1".to_string(),
            harness.to_string(),
        ))
    }

    #[test]
    fn each_harness_hands_on_its_session_id_once_as_soon_as_it_says_it() {
        let cases: Vec<(Box<dyn HarnessAdapter>, Vec<Value>, Option<&str>)> = vec![
            (
                Box::new(ClaudeCodeAdapter::new()),
                vec![
                    serde_json::json!({ "type": "system", "subtype": "init", "session_id": "c-1" }),
                    serde_json::json!({ "type": "assistant", "message": { "content": [] } }),
                ],
                Some("c-1"),
            ),
            (
                Box::new(CodexAdapter::new()),
                vec![
                    serde_json::json!({ "type": "thread.started", "thread_id": "t-1" }),
                    serde_json::json!({ "type": "turn.started" }),
                ],
                Some("t-1"),
            ),
            (
                Box::new(OpenCodeAdapter::new()),
                vec![
                    serde_json::json!({ "type": "step_start", "sessionID": "o-1" }),
                    serde_json::json!({ "type": "tool_use", "sessionID": "o-1" }),
                    serde_json::json!({ "type": "text", "sessionID": "o-1", "text": "hi" }),
                ],
                Some("o-1"),
            ),
            (
                Box::new(GitHubCopilotAdapter::new()),
                vec![
                    serde_json::json!({ "type": "session", "session_id": "g-1" }),
                    serde_json::json!({ "type": "assistant", "content": "hi" }),
                ],
                Some("g-1"),
            ),
            (
                Box::new(ClaudeCodeAdapter::new()),
                vec![serde_json::json!({ "type": "assistant", "message": { "content": [] } })],
                None,
            ),
        ];
        for (adapter, lines, expected) in cases {
            let sessions = Arc::new(RecordedSessions::default());
            let conversation = tapped(&sessions, adapter.name());
            let mut parser = adapter.stream_parser(AnswerShape::Choice);
            for (index, line) in lines.iter().enumerate() {
                let line = line.to_string();
                let elements = parser.on_stdout_line(line.as_bytes());
                record_stdout_line(
                    parser.as_mut(),
                    &conversation,
                    None,
                    line.as_bytes(),
                    &elements,
                    None,
                );
                if index == 0 {
                    // Kept with the line that said it, before any other.
                    assert_eq!(
                        sessions.0.lock().unwrap().len(),
                        usize::from(expected.is_some()),
                        "{}",
                        adapter.name()
                    );
                }
            }
            let recorded = sessions.0.lock().unwrap().clone();
            let expected: Vec<(String, ReportedSession)> = expected
                .map(|id| {
                    (
                        "a1".to_string(),
                        ReportedSession {
                            harness: adapter.name().to_string(),
                            harness_session_id: id.to_string(),
                        },
                    )
                })
                .into_iter()
                .collect();
            assert_eq!(recorded, expected, "{}", adapter.name());
        }
    }

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
