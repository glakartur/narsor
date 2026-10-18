//! Turns one Claude Code `stream-json` line into chat elements.
//!
//! The format is the one recorded in `tests/fixtures/claude-code/` (see the
//! README there). Every line gives at least one element; a line or content
//! block of a shape this module does not know gives
//! [`ChatElement::Unclassified`], never an error.

use std::collections::HashMap;

use narsor_protocol::chat::{
    summary_line, ChatElement, QuestionSource, TechnicalCategory, ToolResultStatus,
    UnclassifiedReason,
};
use serde_json::{Map, Value};

use super::tools::{call_summary, questions};
use super::PermissionDenial;

/// Upper bound, in bytes, of a tool result preview. The raw line keeps the
/// full result; a few KiB show what a tool returned without bloating every
/// conversation page with whole files.
const TOOL_RESULT_PREVIEW_BYTES: usize = 4 * 1024;

/// Upper bound, in bytes, of the serialized `data` kept for a `system`
/// line this module has no rule for.
const SYSTEM_DATA_MAX_BYTES: usize = 1024;

/// The tool name Claude Code uses for a question to the user.
const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

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

/// What a `result` line says about how the run ended — read once, for both
/// the run ([`super::ClaudeCodeStreamParser`]) and its
/// [`ChatElement::RunResult`].
#[derive(Debug)]
pub(super) struct ResultLine {
    /// The line's `subtype`, e.g. `success` or `error_max_turns`.
    pub(super) subtype: Option<String>,
    /// `is_error`, or a `subtype` naming anything other than `success`.
    pub(super) is_error: bool,
    /// `result`, or the non-empty `errors` joined when the run ended without
    /// one (as `error_max_turns` does).
    pub(super) text: Option<String>,
    /// The `structured_output`, when it is there and not `null`.
    pub(super) structured_output: Option<Value>,
    /// Permission denials from the final result, if structurally complete.
    pub(super) permission_denials: Vec<PermissionDenial>,
}

impl ResultLine {
    /// The outcome of a `result` line, moving its `result` and
    /// `structured_output` out of `line`; `None` for any other line.
    pub(super) fn take(line: &mut Map<String, Value>) -> Option<Self> {
        if str_field(line, "type") != Some("result") {
            return None;
        }
        let subtype = str_field(line, "subtype").map(str::to_string);
        let mut is_error = line
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || subtype
                .as_deref()
                .is_some_and(|subtype| subtype != "success");
        let text = match line.remove("result") {
            Some(Value::String(text)) => Some(text),
            _ => errors_text(line),
        };
        let structured_output = line
            .remove("structured_output")
            .filter(|value| !value.is_null());
        let permission_denials: Vec<PermissionDenial> = line
            .get("permission_denials")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some(PermissionDenial {
                            tool_name: entry.get("tool_name")?.as_str()?.to_string(),
                            tool_use_id: entry.get("tool_use_id")?.as_str()?.to_string(),
                            input: entry.get("tool_input")?.clone(),
                            reason_type: entry
                                .get("decision_reason_type")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            reason: entry
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            session_id: line
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Claude Code can report subtype `success` after recovering from a
        // denied tool call. Until Narsor has handled that denial, the run is
        // not a completed success.
        is_error |= !permission_denials.is_empty();
        Some(Self {
            subtype,
            is_error,
            text,
            structured_output,
            permission_denials,
        })
    }

    /// The `subtype`, when it names anything other than `success`.
    fn failed_subtype(&self) -> Option<&str> {
        self.subtype
            .as_deref()
            .filter(|subtype| *subtype != "success")
    }
}

/// The non-empty `errors` of a `result` line, joined.
fn errors_text(line: &Map<String, Value>) -> Option<String> {
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
}

/// One element of a `result` line's `errors` array, as readable text.
/// Claude Code 2.1.302 sends plain strings; older versions sent objects
/// with a `message`.
fn error_message(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
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

/// The string `name` of a content block, empty when it has none.
fn block_str<'a>(block: &'a Value, name: &str) -> &'a str {
    block.get(name).and_then(Value::as_str).unwrap_or_default()
}

impl Classifier {
    /// The elements of one parsed line, in order. `result` is what
    /// [`ResultLine::take`] took out of a `result` line.
    ///
    /// Takes the line by value, so the large parts of it (a tool's input, a
    /// result's usage) move into the elements instead of being copied.
    pub(super) fn classify(
        &mut self,
        mut line: Map<String, Value>,
        result: Option<ResultLine>,
    ) -> Vec<ChatElement> {
        if let Some(result) = result {
            return vec![run_result(line, result)];
        }
        match str_field(&line, "type").unwrap_or_default() {
            "assistant" => self.assistant(take_message_content(&mut line)),
            "user" => self.user(take_message_content(&mut line)),
            "system" => vec![system(&mut line)],
            "rate_limit_event" => vec![rate_limit(&line)],
            _ => unclassified(UnclassifiedReason::UnknownType),
        }
    }

    fn assistant(&mut self, content: Option<Value>) -> Vec<ChatElement> {
        let Some(Value::Array(blocks)) = content else {
            return unclassified(UnclassifiedReason::UnknownBlock);
        };
        blocks
            .into_iter()
            .map(|block| self.assistant_block(block))
            .collect()
    }

    fn assistant_block(&mut self, block: Value) -> ChatElement {
        match block_str(&block, "type") {
            "text" => ChatElement::AgentText {
                text: block_str(&block, "text").to_string(),
            },
            // Claude Code 2.1.302 streams thinking blocks with an empty text
            // and a signature only: the reasoning is withheld.
            "thinking" => {
                let text = block_str(&block, "thinking").to_string();
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

    fn tool_use(&mut self, mut block: Value) -> ChatElement {
        let call_id = block.get("id").and_then(Value::as_str).map(str::to_string);
        let tool = block_str(&block, "name").to_string();
        let input = block
            .as_object_mut()
            .and_then(|block| block.remove("input"))
            .unwrap_or(Value::Null);

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
            &summary_line(&first),
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
    fn user(&self, content: Option<Value>) -> Vec<ChatElement> {
        match content {
            Some(Value::String(text)) => vec![ChatElement::UserMessage { text }],
            Some(Value::Array(blocks)) => {
                blocks.iter().map(|block| self.user_block(block)).collect()
            }
            _ => unclassified(UnclassifiedReason::UnknownBlock),
        }
    }

    fn user_block(&self, block: &Value) -> ChatElement {
        match block_str(block, "type") {
            "text" => ChatElement::UserMessage {
                text: block_str(block, "text").to_string(),
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

/// The `message.content` of a line, moved out of it.
fn take_message_content(line: &mut Map<String, Value>) -> Option<Value> {
    line.get_mut("message")?.as_object_mut()?.remove("content")
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

/// A `system` line; its `compact_metadata` moves into the element.
fn system(line: &mut Map<String, Value>) -> ChatElement {
    let subtype = str_field(line, "subtype").unwrap_or_default().to_string();
    let technical = |category, summary: String, data| ChatElement::Technical {
        category,
        summary: summary_line(&summary),
        data,
    };
    match subtype.as_str() {
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
            let metadata = line.remove("compact_metadata");
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

/// The `result` line: the run result.
///
/// An `awaiting_input` structured output is not turned into a question
/// here: the executor records the question of the answer it accepts, from
/// whichever source it read it.
fn run_result(mut line: Map<String, Value>, result: ResultLine) -> ChatElement {
    ChatElement::RunResult {
        stop_reason: stop_reason(&line, result.failed_subtype()),
        is_error: result.is_error,
        cost_usd: line.get("total_cost_usd").and_then(Value::as_f64),
        usage: line.remove("usage").filter(|usage| !usage.is_null()),
        turns: line.get("num_turns").and_then(Value::as_u64),
        duration_ms: line.get("duration_ms").and_then(Value::as_u64),
        text: result.text,
        structured_output: result.structured_output,
    }
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
