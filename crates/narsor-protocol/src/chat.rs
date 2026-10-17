//! The harness-independent model of a conversation with an agent.
//!
//! Each harness speaks its own stream format. Its stream parser turns every
//! stdout line into zero, one or several [`ChatElement`]s, and the executor
//! adds elements for the lines Narsor writes itself (the prompt, stderr, the
//! process outcome). The elements are stored next to the raw line, so the
//! API, the TUI and the CLI read elements only and never classify harness
//! output on their own. The raw line stays the lossless diagnostic record.
//!
//! Every enum here crosses component boundaries and therefore carries a unit
//! `Unrecognized` variant: an element or value from a newer server degrades
//! to "unknown" in an older client instead of failing the whole page.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upper bound, in characters, of [`ChatElement::summary`].
pub const SUMMARY_MAX_CHARS: usize = 200;

/// Appended to a summary cut at [`SUMMARY_MAX_CHARS`].
const SUMMARY_ELLIPSIS: char = '…';

/// One element of a conversation, as the operator sees it.
///
/// Serialized with an internal `kind` tag, e.g.
/// `{"kind":"agent_text","text":"Done."}`. Optional fields are omitted when
/// absent. An unknown `kind` deserializes to [`ChatElement::Unrecognized`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatElement {
    /// The prompt Narsor sent to the agent.
    Prompt {
        text: String,
        /// The harness session the prompt continues, when it resumes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed_session: Option<String>,
        /// Why Narsor sent the prompt.
        purpose: PromptPurpose,
    },
    /// A user message the harness reported as sent to the agent.
    UserMessage { text: String },
    /// The agent's reasoning, stored in full.
    Thinking {
        text: String,
        /// True when the harness withheld the reasoning text.
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
    },
    /// Text the agent wrote, stored in full.
    AgentText { text: String },
    /// A question the agent asked the operator.
    AgentQuestion {
        question: String,
        /// Suggested answers, if the agent offered any.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
        /// Where the question came from.
        source: QuestionSource,
        /// The tool call that asked, when [`QuestionSource::Tool`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// A tool the agent invoked.
    ToolCall {
        /// Correlates the call with its [`ChatElement::ToolResult`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// Tool name as the harness reports it, e.g. `Read` or `Skill`.
        tool: String,
        /// The key parameter on one line (a path, a command, a skill name).
        /// May be empty when the tool has no obvious key parameter.
        #[serde(default)]
        summary: String,
        /// The full tool input.
        #[serde(default)]
        input: Value,
    },
    /// The outcome of a tool call.
    ///
    /// Carries the tool name and call summary copied from the call, so a
    /// consumer never has to look for the call on another page.
    ToolResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_summary: Option<String>,
        status: ToolResultStatus,
        /// A bounded preview of the result. The raw line keeps it in full.
        #[serde(default)]
        content: String,
        /// True when `content` was cut to the preview limit.
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// Metadata the harness reported when the session started.
    SessionStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cli_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
    },
    /// The final result of the agent run, with its metadata.
    RunResult {
        /// The final text answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The answer that matched the output schema, when one was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_output: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Token usage exactly as the harness reported it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// A technical event without conversation content.
    Technical {
        category: TechnicalCategory,
        /// One line describing the event.
        #[serde(default)]
        summary: String,
        /// Event details worth keeping, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
    /// A line the parser could not classify. Never a run failure.
    Unclassified { reason: UnclassifiedReason },
    /// An element kind this build does not know, sent by a newer server.
    #[serde(other)]
    Unrecognized,
}

/// Why Narsor sent a prompt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptPurpose {
    /// The prompt of a workflow step.
    Step,
    /// A follow-up asking the agent to fix an answer that broke the output
    /// contract.
    Reprompt,
    /// A prompt to the conductor.
    Conductor,
    #[serde(other)]
    Unrecognized,
}

/// Where an agent question came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuestionSource {
    /// An `awaiting_input` answer under the output contract.
    OutputContract,
    /// A question tool, e.g. Claude Code's `AskUserQuestion`.
    Tool,
    #[serde(other)]
    Unrecognized,
}

/// Whether a tool call succeeded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Ok,
    Error,
    #[serde(other)]
    Unrecognized,
}

impl ToolResultStatus {
    /// Stable lowercase label, the same as the wire value.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Unrecognized => "unknown",
        }
    }
}

/// The kind of a [`ChatElement::Technical`] event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TechnicalCategory {
    /// A harness hook ran.
    Hook,
    /// The harness reported a rate limit.
    RateLimit,
    /// The harness compacted the context.
    Compaction,
    /// Any other harness system message.
    System,
    /// A line the harness wrote to stderr.
    Stderr,
    /// The harness process outcome: exit code, timeout, spawn failure.
    Process,
    #[serde(other)]
    Unrecognized,
}

impl TechnicalCategory {
    /// Stable lowercase label, the same as the wire value.
    pub fn label(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::RateLimit => "rate_limit",
            Self::Compaction => "compaction",
            Self::System => "system",
            Self::Stderr => "stderr",
            Self::Process => "process",
            Self::Unrecognized => "unknown",
        }
    }
}

/// Why a line became [`ChatElement::Unclassified`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnclassifiedReason {
    /// Valid JSON with a message type the parser does not know.
    UnknownType,
    /// The line is not valid JSON.
    InvalidJson,
    /// The line is longer than the parse limit and was not read.
    TooLarge,
    /// The harness adapter does not classify its output yet.
    HarnessNotClassified,
    /// A content block the parser does not know.
    UnknownBlock,
    /// The entry was recorded before Narsor stored elements with it, so
    /// only its raw line exists.
    NotRecorded,
    /// The element was recorded, but this build cannot read it back: a
    /// field is missing or has another type, or it is nested too deeply.
    Unreadable,
    #[serde(other)]
    Unrecognized,
}

impl UnclassifiedReason {
    /// Short human-readable description.
    pub fn description(self) -> &'static str {
        match self {
            Self::UnknownType => "unknown message type",
            Self::InvalidJson => "not valid JSON",
            Self::TooLarge => "line too large to parse",
            Self::HarnessNotClassified => "harness output not classified",
            Self::UnknownBlock => "unknown content block",
            Self::NotRecorded => "recorded without elements",
            Self::Unreadable => "recorded element not readable",
            Self::Unrecognized => "unrecognized reason",
        }
    }
}

/// The higher-level group of a [`ChatElement`], for display and filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatCategory {
    /// Sent to the agent: prompts and user messages.
    SentToAgent,
    Thinking,
    AgentText,
    AgentQuestion,
    /// Tool calls and tool results.
    Tool,
    /// The final run result.
    Result,
    /// Session metadata and technical events.
    Technical,
    /// Unclassified lines and unknown element kinds.
    Unclassified,
}

impl ChatCategory {
    /// Every category, in conversation display order.
    #[cfg(test)]
    pub const ALL: [ChatCategory; 8] = [
        Self::SentToAgent,
        Self::Thinking,
        Self::AgentText,
        Self::AgentQuestion,
        Self::Tool,
        Self::Result,
        Self::Technical,
        Self::Unclassified,
    ];

    /// Short stable label for the TUI and CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::SentToAgent => "sent",
            Self::Thinking => "thinking",
            Self::AgentText => "agent",
            Self::AgentQuestion => "question",
            Self::Tool => "tool",
            Self::Result => "result",
            Self::Technical => "technical",
            Self::Unclassified => "unclassified",
        }
    }
}

impl ChatElement {
    /// The higher-level group the element belongs to.
    ///
    /// [`ChatElement::SessionStarted`] is [`ChatCategory::Technical`]; an
    /// unknown element kind is [`ChatCategory::Unclassified`].
    pub fn category(&self) -> ChatCategory {
        match self {
            Self::Prompt { .. } | Self::UserMessage { .. } => ChatCategory::SentToAgent,
            Self::Thinking { .. } => ChatCategory::Thinking,
            Self::AgentText { .. } => ChatCategory::AgentText,
            Self::AgentQuestion { .. } => ChatCategory::AgentQuestion,
            Self::ToolCall { .. } | Self::ToolResult { .. } => ChatCategory::Tool,
            Self::RunResult { .. } => ChatCategory::Result,
            Self::SessionStarted { .. } | Self::Technical { .. } => ChatCategory::Technical,
            Self::Unclassified { .. } | Self::Unrecognized => ChatCategory::Unclassified,
        }
    }

    /// A one-line description of the element, at most
    /// [`SUMMARY_MAX_CHARS`] characters.
    ///
    /// Text elements give their first non-blank line; a tool call gives the
    /// tool and its key parameter (`Skill ce-plan`); a tool result gives the
    /// call and its status. The full content stays in the element.
    pub fn summary(&self) -> String {
        summary_line(&self.unbounded_summary())
    }

    fn unbounded_summary(&self) -> String {
        match self {
            Self::Prompt { text, purpose, .. } => match purpose {
                PromptPurpose::Step => first_line(text).to_owned(),
                PromptPurpose::Reprompt => format!("reprompt: {}", first_line(text)),
                PromptPurpose::Conductor => format!("conductor: {}", first_line(text)),
                PromptPurpose::Unrecognized => first_line(text).to_owned(),
            },
            Self::UserMessage { text } | Self::AgentText { text } => first_line(text).to_owned(),
            Self::Thinking { text, redacted } => {
                if *redacted && text.trim().is_empty() {
                    "(redacted)".to_owned()
                } else {
                    first_line(text).to_owned()
                }
            }
            Self::AgentQuestion {
                question, options, ..
            } => {
                let question = first_line(question);
                if options.is_empty() {
                    question.to_owned()
                } else {
                    format!("{question} [{}]", options.join(" | "))
                }
            }
            Self::ToolCall { tool, summary, .. } => join_nonempty(&[tool, summary]),
            Self::ToolResult {
                call_id,
                tool,
                call_summary,
                status,
                content,
                ..
            } => {
                let mut call = join_nonempty(&[
                    tool.as_deref().unwrap_or_default(),
                    call_summary.as_deref().unwrap_or_default(),
                ]);
                if call.is_empty() {
                    call = call_id.clone().unwrap_or_else(|| "tool".to_owned());
                }
                let mut line = format!("{call} — {}", status.label());
                if *status == ToolResultStatus::Error && !first_line(content).is_empty() {
                    line.push_str(": ");
                    line.push_str(first_line(content));
                }
                line
            }
            Self::SessionStarted {
                session_id,
                model,
                cli_version,
                ..
            } => {
                let mut parts = vec!["session started".to_owned()];
                parts.extend(model.clone());
                parts.extend(cli_version.as_ref().map(|v| format!("cli {v}")));
                parts.extend(session_id.as_ref().map(|id| format!("session {id}")));
                parts.join(" · ")
            }
            Self::RunResult {
                stop_reason,
                is_error,
                cost_usd,
                turns,
                duration_ms,
                ..
            } => {
                let mut parts = vec![if *is_error { "error" } else { "done" }.to_owned()];
                parts.extend(stop_reason.clone());
                parts.extend(turns.map(|t| format!("{t} turns")));
                parts.extend(duration_ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0)));
                parts.extend(cost_usd.map(|c| format!("${c:.4}")));
                parts.join(" · ")
            }
            Self::Technical {
                category, summary, ..
            } => match first_line(summary) {
                "" => category.label().to_owned(),
                line => format!("{}: {line}", category.label()),
            },
            Self::Unclassified { reason } => reason.description().to_owned(),
            Self::Unrecognized => "element from a newer version".to_owned(),
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The first line of `text` that is not blank, trimmed.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
}

/// The non-empty parts, trimmed and joined with a space.
fn join_nonempty(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` as one summary line: every run of whitespace and control
/// characters (line breaks included) becomes one space, the ends are
/// trimmed, and a line longer than [`SUMMARY_MAX_CHARS`] characters is cut
/// to that length, its last character replaced by `…`.
///
/// Reads `text` only as far as the summary needs, so a huge input costs no
/// more than a short one.
pub fn summary_line(text: &str) -> String {
    let mut line: Vec<char> = Vec::with_capacity(SUMMARY_MAX_CHARS + 2);
    let mut gap = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            gap = !line.is_empty();
            continue;
        }
        if gap {
            line.push(' ');
            gap = false;
        }
        line.push(c);
        if line.len() > SUMMARY_MAX_CHARS {
            break;
        }
    }
    if line.len() > SUMMARY_MAX_CHARS {
        line.truncate(SUMMARY_MAX_CHARS - 1);
        line.push(SUMMARY_ELLIPSIS);
    }
    line.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn every_variant() -> Vec<ChatElement> {
        vec![
            ChatElement::Prompt {
                text: "Plan the ticket.".into(),
                resumed_session: Some("sess-1".into()),
                purpose: PromptPurpose::Reprompt,
            },
            ChatElement::UserMessage {
                text: "hello".into(),
            },
            ChatElement::Thinking {
                text: "Let me think.".into(),
                redacted: true,
            },
            ChatElement::AgentText {
                text: "Done.\nDetails follow.".into(),
            },
            ChatElement::AgentQuestion {
                question: "Which branch?".into(),
                options: vec!["main".into(), "dev".into()],
                source: QuestionSource::Tool,
                call_id: Some("toolu_1".into()),
            },
            ChatElement::ToolCall {
                call_id: Some("toolu_2".into()),
                tool: "Skill".into(),
                summary: "ce-plan".into(),
                input: json!({"skill": "ce-plan", "args": "x"}),
            },
            ChatElement::ToolResult {
                call_id: Some("toolu_2".into()),
                tool: Some("Skill".into()),
                call_summary: Some("ce-plan".into()),
                status: ToolResultStatus::Error,
                content: "boom".into(),
                truncated: true,
            },
            ChatElement::SessionStarted {
                session_id: Some("sess-1".into()),
                model: Some("claude-test".into()),
                cli_version: Some("2.1.302".into()),
                cwd: Some("/work".into()),
                permission_mode: Some("plan".into()),
            },
            ChatElement::RunResult {
                text: Some("ok".into()),
                structured_output: Some(json!({"output": "done"})),
                stop_reason: Some("end_turn".into()),
                is_error: true,
                cost_usd: Some(0.125),
                usage: Some(json!({"input_tokens": 10, "output_tokens": 5})),
                turns: Some(3),
                duration_ms: Some(1500),
            },
            ChatElement::Technical {
                category: TechnicalCategory::RateLimit,
                summary: "five_hour allowed".into(),
                data: Some(json!({"status": "allowed"})),
            },
            ChatElement::Unclassified {
                reason: UnclassifiedReason::UnknownBlock,
            },
            ChatElement::Unrecognized,
        ]
    }

    #[test]
    fn every_variant_round_trips_unchanged() {
        for element in every_variant() {
            let wire = serde_json::to_string(&element).unwrap();
            let back: ChatElement = serde_json::from_str(&wire).unwrap();
            assert_eq!(back, element, "{wire}");
        }
    }

    #[test]
    fn elements_use_a_snake_case_kind_tag() {
        let wire = serde_json::to_value(ChatElement::AgentText { text: "hi".into() }).unwrap();
        assert_eq!(wire, json!({"kind": "agent_text", "text": "hi"}));
        let wire = serde_json::to_value(ChatElement::Unrecognized).unwrap();
        assert_eq!(wire, json!({"kind": "unrecognized"}));
    }

    #[test]
    fn unknown_kinds_keep_the_forward_compatible_fallback() {
        let element: ChatElement =
            serde_json::from_str(r#"{"kind":"future_thing","x":1}"#).unwrap();
        assert_eq!(element, ChatElement::Unrecognized);
    }

    #[test]
    fn unknown_nested_values_keep_the_forward_compatible_fallback() {
        let element: ChatElement =
            serde_json::from_str(r#"{"kind":"technical","category":"telemetry","summary":"x"}"#)
                .unwrap();
        assert_eq!(element.category(), ChatCategory::Technical);
        assert!(matches!(
            element,
            ChatElement::Technical {
                category: TechnicalCategory::Unrecognized,
                ..
            }
        ));

        let reason: UnclassifiedReason = serde_json::from_str("\"new_reason\"").unwrap();
        assert_eq!(reason, UnclassifiedReason::Unrecognized);
        let purpose: PromptPurpose = serde_json::from_str("\"new_purpose\"").unwrap();
        assert_eq!(purpose, PromptPurpose::Unrecognized);
        let source: QuestionSource = serde_json::from_str("\"new_source\"").unwrap();
        assert_eq!(source, QuestionSource::Unrecognized);
        let status: ToolResultStatus = serde_json::from_str("\"partial\"").unwrap();
        assert_eq!(status, ToolResultStatus::Unrecognized);
    }

    #[test]
    fn missing_optional_fields_are_none_and_not_serialized() {
        let wire = json!({"kind": "session_started"});
        let element: ChatElement = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            element,
            ChatElement::SessionStarted {
                session_id: None,
                model: None,
                cli_version: None,
                cwd: None,
                permission_mode: None,
            }
        );
        assert_eq!(serde_json::to_value(&element).unwrap(), wire);

        let wire = json!({"kind": "run_result"});
        let element: ChatElement = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(
            element,
            ChatElement::RunResult {
                text: None,
                structured_output: None,
                is_error: false,
                cost_usd: None,
                usage: None,
                ..
            }
        ));
        assert_eq!(serde_json::to_value(&element).unwrap(), wire);

        let wire = json!({"kind": "tool_result", "status": "ok", "content": ""});
        let element: ChatElement = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&element).unwrap(), wire);
    }

    #[test]
    fn a_missing_required_field_is_an_error() {
        let result = serde_json::from_str::<ChatElement>(r#"{"kind":"agent_text"}"#);
        assert!(result.is_err());
        let result = serde_json::from_str::<ChatElement>(r#"{"text":"no kind"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn tool_call_summary_names_the_tool_and_its_key_parameter() {
        let element = ChatElement::ToolCall {
            call_id: None,
            tool: "Skill".into(),
            summary: "ce-plan".into(),
            input: json!({"skill": "ce-plan"}),
        };
        let summary = element.summary();
        assert!(summary.contains("Skill"), "{summary}");
        assert!(summary.contains("ce-plan"), "{summary}");
        assert_eq!(element.category(), ChatCategory::Tool);
    }

    #[test]
    fn tool_result_summary_names_the_call_and_its_status() {
        let element = ChatElement::ToolResult {
            call_id: Some("toolu_1".into()),
            tool: Some("Bash".into()),
            call_summary: Some("cargo test".into()),
            status: ToolResultStatus::Error,
            content: "\nexit code 101\nmore".into(),
            truncated: false,
        };
        assert_eq!(element.summary(), "Bash cargo test — error: exit code 101");

        let anonymous = ChatElement::ToolResult {
            call_id: Some("toolu_9".into()),
            tool: None,
            call_summary: None,
            status: ToolResultStatus::Ok,
            content: String::new(),
            truncated: false,
        };
        assert_eq!(anonymous.summary(), "toolu_9 — ok");
    }

    #[test]
    fn summaries_are_single_line_and_bounded() {
        let element = ChatElement::AgentText {
            text: "\n\n  first line\tend  \nsecond line".into(),
        };
        assert_eq!(element.summary(), "first line end");

        let long = ChatElement::ToolCall {
            call_id: None,
            tool: "Bash".into(),
            summary: "x".repeat(SUMMARY_MAX_CHARS * 2),
            input: Value::Null,
        };
        let summary = long.summary();
        assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS);
        assert!(summary.ends_with(SUMMARY_ELLIPSIS));

        for element in every_variant() {
            let summary = element.summary();
            assert!(!summary.contains('\n'), "{summary}");
            assert!(summary.chars().count() <= SUMMARY_MAX_CHARS);
        }
    }

    #[test]
    fn a_summary_line_collapses_whitespace_and_control_characters() {
        assert_eq!(summary_line("  a \t\x1b b\n\nc  "), "a b c");
        assert_eq!(summary_line(""), "");

        let exact = "x".repeat(SUMMARY_MAX_CHARS);
        assert_eq!(summary_line(&exact), exact);

        let cut = summary_line(&format!("{exact} y"));
        assert_eq!(cut.chars().count(), SUMMARY_MAX_CHARS);
        assert!(cut.ends_with(SUMMARY_ELLIPSIS));
        assert_eq!(summary_line(&cut), cut);
    }

    #[test]
    fn unclassified_summary_gives_the_reason() {
        let element = ChatElement::Unclassified {
            reason: UnclassifiedReason::TooLarge,
        };
        assert_eq!(element.summary(), "line too large to parse");

        let legacy: ChatElement =
            serde_json::from_str(r#"{"kind":"unclassified","reason":"not_recorded"}"#).unwrap();
        assert_eq!(
            legacy,
            ChatElement::Unclassified {
                reason: UnclassifiedReason::NotRecorded
            }
        );
        assert_eq!(legacy.summary(), "recorded without elements");

        let unreadable: ChatElement =
            serde_json::from_str(r#"{"kind":"unclassified","reason":"unreadable"}"#).unwrap();
        assert_eq!(
            unreadable,
            ChatElement::Unclassified {
                reason: UnclassifiedReason::Unreadable
            }
        );
        assert_eq!(
            serde_json::to_string(&unreadable).unwrap(),
            r#"{"kind":"unclassified","reason":"unreadable"}"#
        );
        assert_eq!(unreadable.summary(), "recorded element not readable");
    }

    #[test]
    fn every_element_maps_to_its_category() {
        let categories: Vec<_> = every_variant().iter().map(ChatElement::category).collect();
        assert_eq!(
            categories,
            vec![
                ChatCategory::SentToAgent,
                ChatCategory::SentToAgent,
                ChatCategory::Thinking,
                ChatCategory::AgentText,
                ChatCategory::AgentQuestion,
                ChatCategory::Tool,
                ChatCategory::Tool,
                ChatCategory::Technical,
                ChatCategory::Result,
                ChatCategory::Technical,
                ChatCategory::Unclassified,
                ChatCategory::Unclassified,
            ]
        );
        let labels: Vec<_> = ChatCategory::ALL.iter().map(|c| c.label()).collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }
}
