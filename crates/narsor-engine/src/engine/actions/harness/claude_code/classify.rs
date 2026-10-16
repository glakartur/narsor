//! Turns one Claude Code `stream-json` line into chat elements.
//!
//! The format is the one recorded in `tests/fixtures/claude-code/` (see the
//! README there). Every line gives at least one element; a line or content
//! block of a shape this module does not know gives
//! [`ChatElement::Unclassified`], never an error.

use std::collections::HashMap;

use narsor_protocol::chat::{
    ChatElement, QuestionSource, TechnicalCategory, ToolResultStatus, UnclassifiedReason,
};
use serde_json::{Map, Value};

use super::error_message;
use super::tools::{call_summary, one_line, questions};

/// Upper bound, in bytes, of a tool result preview. The raw line keeps the
/// full result; a few KiB show what a tool returned without bloating every
/// conversation page with whole files.
const TOOL_RESULT_PREVIEW_BYTES: usize = 4 * 1024;

/// Upper bound, in bytes, of the serialized `data` kept for a `system`
/// line this module has no rule for.
const SYSTEM_DATA_MAX_BYTES: usize = 1024;

/// The tool name Claude Code uses for a question to the user.
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

/// The `structured_output.status` of an answer that asks the operator.
const AWAITING_INPUT: &str = "awaiting_input";

/// What a tool result needs to know about its call.
#[derive(Debug)]
struct Call {
    tool: String,
    summary: String,
}

/// Classifies the lines of one run. Remembers each tool call by id, so a
/// later `tool_result` names its tool and call summary.
#[derive(Debug, Default)]
pub(super) struct Classifier {
    calls: HashMap<String, Call>,
}

/// The element of a line that is not a JSON object.
pub(super) fn invalid_json() -> Vec<ChatElement> {
    unclassified(UnclassifiedReason::InvalidJson)
}

fn unclassified(reason: UnclassifiedReason) -> Vec<ChatElement> {
    vec![ChatElement::Unclassified { reason }]
}

fn str_field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    object.get(name).and_then(Value::as_str)
}

impl Classifier {
    /// The elements of one parsed line, in order.
    pub(super) fn classify(&mut self, line: &Map<String, Value>) -> Vec<ChatElement> {
        match str_field(line, "type").unwrap_or_default() {
            "assistant" => self.assistant(line),
            "user" => self.user(line),
            "system" => vec![system(line)],
            "rate_limit_event" => vec![rate_limit(line)],
            "result" => result(line),
            _ => unclassified(UnclassifiedReason::UnknownType),
        }
    }

    fn assistant(&mut self, line: &Map<String, Value>) -> Vec<ChatElement> {
        let Some(blocks) = message_content(line).and_then(Value::as_array) else {
            return unclassified(UnclassifiedReason::UnknownBlock);
        };
        blocks
            .iter()
            .map(|block| self.assistant_block(block))
            .collect()
    }

    fn assistant_block(&mut self, block: &Value) -> ChatElement {
        let text = |name: &str| {
            block
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => ChatElement::AgentText { text: text("text") },
            // Claude Code 2.1.302 streams thinking blocks with an empty text
            // and a signature only: the reasoning is withheld.
            "thinking" => {
                let text = text("thinking");
                let redacted = text.is_empty();
                ChatElement::Thinking { text, redacted }
            }
            "redacted_thinking" => ChatElement::Thinking {
                text: String::new(),
                redacted: true,
            },
            "tool_use" => self.tool_use(block),
            _ => ChatElement::Unclassified {
                reason: UnclassifiedReason::UnknownBlock,
            },
        }
    }

    fn tool_use(&mut self, block: &Value) -> ChatElement {
        let call_id = block.get("id").and_then(Value::as_str).map(str::to_string);
        let tool = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let input = block.get("input").cloned().unwrap_or(Value::Null);

        if tool == ASK_USER_QUESTION_TOOL {
            if let Some(element) = self.ask_user_question(call_id.clone(), &input) {
                return element;
            }
        }

        let summary = call_summary(&tool, &input);
        self.remember(call_id.as_deref(), &tool, &summary);
        ChatElement::ToolCall {
            call_id,
            tool,
            summary,
            input,
        }
    }

    /// An `AskUserQuestion` call as a question to the operator. A single
    /// question keeps its options as a list; several questions are written
    /// one per line with their options inline, since one option list cannot
    /// serve them all. `None` when the input holds no question text.
    fn ask_user_question(&mut self, call_id: Option<String>, input: &Value) -> Option<ChatElement> {
        let mut asked = questions(input);
        let first = asked.first()?.0.clone();
        let (question, options) = if asked.len() == 1 {
            asked.remove(0)
        } else {
            let lines = asked
                .into_iter()
                .map(|(question, options)| match options.as_slice() {
                    [] => question,
                    _ => format!("{question} [{}]", options.join(" | ")),
                })
                .collect::<Vec<_>>()
                .join("\n");
            (lines, Vec::new())
        };
        self.remember(
            call_id.as_deref(),
            ASK_USER_QUESTION_TOOL,
            &one_line(&first),
        );
        Some(ChatElement::AgentQuestion {
            question,
            options,
            source: QuestionSource::Tool,
            call_id,
        })
    }

    fn remember(&mut self, call_id: Option<&str>, tool: &str, summary: &str) {
        if let Some(id) = call_id {
            self.calls.insert(
                id.to_string(),
                Call {
                    tool: tool.to_string(),
                    summary: summary.to_string(),
                },
            );
        }
    }

    /// A `user` line: what Claude Code sent to the agent — tool results,
    /// and text such as an injected skill body or the CLI's own
    /// `[structured-output-enforce]` nudge (both `isSynthetic`).
    fn user(&self, line: &Map<String, Value>) -> Vec<ChatElement> {
        match message_content(line) {
            Some(Value::String(text)) => vec![ChatElement::UserMessage { text: text.clone() }],
            Some(Value::Array(blocks)) => {
                blocks.iter().map(|block| self.user_block(block)).collect()
            }
            _ => unclassified(UnclassifiedReason::UnknownBlock),
        }
    }

    fn user_block(&self, block: &Value) -> ChatElement {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => ChatElement::UserMessage {
                text: block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "tool_result" => self.tool_result(block),
            _ => ChatElement::Unclassified {
                reason: UnclassifiedReason::UnknownBlock,
            },
        }
    }

    fn tool_result(&self, block: &Value) -> ChatElement {
        let call_id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        // A call from before a resumed session is not in the map.
        let call = call_id.as_deref().and_then(|id| self.calls.get(id));
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (content, truncated) = preview(&result_text(block.get("content")));
        ChatElement::ToolResult {
            call_id,
            tool: call.map(|call| call.tool.clone()),
            call_summary: call.map(|call| call.summary.clone()),
            status: if is_error {
                ToolResultStatus::Error
            } else {
                ToolResultStatus::Ok
            },
            content,
            truncated,
        }
    }
}

fn message_content(line: &Map<String, Value>) -> Option<&Value> {
    line.get("message")?.get("content")
}

/// A tool result's content as text: a string as is, text blocks joined by
/// line breaks, any other block as `[<type>]`.
fn result_text(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match block.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => format!(
                    "[{}]",
                    block.get("type").and_then(Value::as_str).unwrap_or("block")
                ),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

/// `text` cut to [`TOOL_RESULT_PREVIEW_BYTES`] on a character boundary,
/// and whether it was cut.
fn preview(text: &str) -> (String, bool) {
    if text.len() <= TOOL_RESULT_PREVIEW_BYTES {
        return (text.to_string(), false);
    }
    let mut end = TOOL_RESULT_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn system(line: &Map<String, Value>) -> ChatElement {
    let subtype = str_field(line, "subtype").unwrap_or_default();
    let technical = |category, summary: String, data| ChatElement::Technical {
        category,
        summary: one_line(&summary),
        data,
    };
    match subtype {
        "init" => ChatElement::SessionStarted {
            session_id: str_field(line, "session_id").map(str::to_string),
            model: str_field(line, "model").map(str::to_string),
            cli_version: str_field(line, "claude_code_version").map(str::to_string),
            cwd: str_field(line, "cwd").map(str::to_string),
            permission_mode: str_field(line, "permissionMode").map(str::to_string),
        },
        hook if hook.starts_with("hook_") => {
            technical(TechnicalCategory::Hook, hook_summary(line, hook), None)
        }
        compaction if compaction.contains("compact") => {
            let metadata = line.get("compact_metadata").cloned();
            technical(
                TechnicalCategory::Compaction,
                compaction_summary(metadata.as_ref()),
                metadata,
            )
        }
        "thinking_tokens" => {
            let summary = match line.get("estimated_tokens").and_then(Value::as_u64) {
                Some(tokens) => format!("thinking ~{tokens} tokens"),
                None => "thinking tokens".to_string(),
            };
            technical(TechnicalCategory::System, summary, None)
        }
        "permission_denied" => {
            let tool = str_field(line, "tool_name").unwrap_or("tool");
            let message = str_field(line, "message").unwrap_or_default();
            let data = serde_json::json!({
                "tool_name": line.get("tool_name"),
                "tool_use_id": line.get("tool_use_id"),
            });
            technical(
                TechnicalCategory::System,
                format!("permission denied: {tool}: {message}"),
                Some(data),
            )
        }
        other => {
            let summary = if other.is_empty() { "system" } else { other };
            technical(
                TechnicalCategory::System,
                summary.to_string(),
                small_remainder(line),
            )
        }
    }
}

/// `<hook name> started`, `<hook name> exit <code>` (with the outcome when
/// it is not `success`), or `<hook name> <event>` for other hook lines.
fn hook_summary(line: &Map<String, Value>, subtype: &str) -> String {
    let name = str_field(line, "hook_name")
        .or_else(|| str_field(line, "hook_event"))
        .unwrap_or("hook");
    match subtype {
        "hook_started" => format!("{name} started"),
        "hook_response" => {
            let mut summary = match line.get("exit_code").and_then(Value::as_i64) {
                Some(code) => format!("{name} exit {code}"),
                None => format!("{name} finished"),
            };
            if let Some(outcome) = str_field(line, "outcome").filter(|o| *o != "success") {
                summary.push_str(&format!(" ({outcome})"));
            }
            summary
        }
        other => format!("{name} {}", other.trim_start_matches("hook_")),
    }
}

/// `context compacted (<trigger>, <n> tokens before)`.
fn compaction_summary(metadata: Option<&Value>) -> String {
    let field = |name: &str| metadata.and_then(|metadata| metadata.get(name));
    let mut details = Vec::new();
    if let Some(trigger) = field("trigger").and_then(Value::as_str) {
        details.push(trigger.to_string());
    }
    if let Some(tokens) = field("pre_tokens").and_then(Value::as_u64) {
        details.push(format!("{tokens} tokens before"));
    }
    match details.as_slice() {
        [] => "context compacted".to_string(),
        _ => format!("context compacted ({})", details.join(", ")),
    }
}

/// The fields of a `system` line without a rule, minus the envelope, when
/// they are small enough to keep.
fn small_remainder(line: &Map<String, Value>) -> Option<Value> {
    let remainder: Map<String, Value> = line
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "type" | "subtype" | "uuid" | "session_id"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if remainder.is_empty() {
        return None;
    }
    let value = Value::Object(remainder);
    (value.to_string().len() <= SYSTEM_DATA_MAX_BYTES).then_some(value)
}

/// `<status> (<limit type>)`, e.g. `allowed (five_hour)`.
fn rate_limit(line: &Map<String, Value>) -> ChatElement {
    let info = line.get("rate_limit_info");
    let field = |name: &str| info.and_then(|info| info.get(name)).and_then(Value::as_str);
    let summary = match (field("status"), field("rateLimitType")) {
        (Some(status), Some(kind)) => format!("{status} ({kind})"),
        (Some(status), None) => status.to_string(),
        (None, Some(kind)) => kind.to_string(),
        (None, None) => "rate limit".to_string(),
    };
    ChatElement::Technical {
        category: TechnicalCategory::RateLimit,
        summary,
        data: info.cloned(),
    }
}

/// The `result` line: the run result, followed by the agent's question
/// when its structured output asks the operator.
fn result(line: &Map<String, Value>) -> Vec<ChatElement> {
    let subtype = str_field(line, "subtype");
    let failed_subtype = subtype.filter(|subtype| *subtype != "success");
    let is_error = line
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || failed_subtype.is_some();
    let structured_output = line
        .get("structured_output")
        .filter(|value| !value.is_null());

    let mut elements = vec![ChatElement::RunResult {
        text: result_line_text(line),
        structured_output: structured_output.cloned(),
        stop_reason: stop_reason(line, failed_subtype),
        is_error,
        cost_usd: line.get("total_cost_usd").and_then(Value::as_f64),
        usage: line.get("usage").filter(|usage| !usage.is_null()).cloned(),
        turns: line.get("num_turns").and_then(Value::as_u64),
        duration_ms: line.get("duration_ms").and_then(Value::as_u64),
    }];
    if let Some(question) = structured_output.and_then(awaiting_input_question) {
        elements.push(question);
    }
    elements
}

/// `result`, or the `errors` joined when the run ended without one (as
/// `error_max_turns` does).
fn result_line_text(line: &Map<String, Value>) -> Option<String> {
    str_field(line, "result").map(str::to_string).or_else(|| {
        line.get("errors")
            .and_then(Value::as_array)
            .filter(|errors| !errors.is_empty())
            .map(|errors| {
                errors
                    .iter()
                    .map(error_message)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
    })
}

/// Why the run ended, most telling first: a non-`success` `subtype`
/// (`error_max_turns`), then a `terminal_reason` other than `completed`
/// (`api_error` arrives with `subtype: success`), then the model's
/// `stop_reason` (`end_turn`, `tool_use`).
fn stop_reason(line: &Map<String, Value>, failed_subtype: Option<&str>) -> Option<String> {
    failed_subtype
        .or_else(|| str_field(line, "terminal_reason").filter(|reason| *reason != "completed"))
        .or_else(|| str_field(line, "stop_reason"))
        .map(str::to_string)
}

/// The question of a structured output with `status: awaiting_input`.
fn awaiting_input_question(output: &Value) -> Option<ChatElement> {
    if output.get("status").and_then(Value::as_str) != Some(AWAITING_INPUT) {
        return None;
    }
    let options = output
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    Some(ChatElement::AgentQuestion {
        question: output
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        options,
        source: QuestionSource::OutputContract,
        call_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preview_cuts_on_a_character_boundary() {
        let text = format!("a{}", "ż".repeat(TOOL_RESULT_PREVIEW_BYTES));
        let (content, truncated) = preview(&text);
        assert!(truncated);
        assert_eq!(content.len(), TOOL_RESULT_PREVIEW_BYTES - 1);
        assert!(text.starts_with(&content));

        let exact = "x".repeat(TOOL_RESULT_PREVIEW_BYTES);
        assert_eq!(preview(&exact), (exact.clone(), false));
    }
}
