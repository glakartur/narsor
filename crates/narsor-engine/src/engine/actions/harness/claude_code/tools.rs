//! The key parameter of a Claude Code tool call, as one short line.
//!
//! A [`ChatElement::ToolCall`](narsor_protocol::chat::ChatElement::ToolCall)
//! shows the tool name next to this summary, so the summary never repeats
//! the name: `Read` gets the path, `Bash` the command, `Skill` the skill.

use narsor_protocol::chat::summary_line;
use serde_json::Value;

/// The name of the tool the CLI adds when it is run with `--json-schema`.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";

/// The key parameter of a call to `tool` with `input`, on one bounded
/// line. Empty for a tool without a rule.
pub(super) fn call_summary(tool: &str, input: &Value) -> String {
    summary_line(&unbounded_summary(tool, input))
}

fn unbounded_summary(tool: &str, input: &Value) -> String {
    let field = |name: &str| input.get(name).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "Bash" => field("command").to_string(),
        "Read" | "Write" | "Edit" => field("file_path").to_string(),
        "NotebookEdit" => field("notebook_path").to_string(),
        "Glob" | "Grep" => match field("path") {
            "" => field("pattern").to_string(),
            path => format!("{} in {path}", field("pattern")),
        },
        "WebFetch" => field("url").to_string(),
        "WebSearch" | "ToolSearch" => field("query").to_string(),
        "Skill" => field("skill").to_string(),
        "Task" | "Agent" => match field("subagent_type") {
            "" => field("description").to_string(),
            kind => format!("{} ({kind})", field("description")),
        },
        "TodoWrite" => {
            let count = input
                .get("todos")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("{count} tasks")
        }
        STRUCTURED_OUTPUT_TOOL => structured_output_summary(input),
        _ => mcp_summary(tool).unwrap_or_default(),
    }
}

/// `choice: <label>`, or `awaiting_input: <question>` — what the agent
/// answered through the output schema.
fn structured_output_summary(input: &Value) -> String {
    let field = |name: &str| input.get(name).and_then(Value::as_str);
    match (field("status"), field("choice")) {
        (Some(status), _) => match field("question") {
            Some(question) => format!("{status}: {question}"),
            None => status.to_string(),
        },
        (None, Some(choice)) => format!("choice: {choice}"),
        (None, None) => String::new(),
    }
}

/// `server/tool` for an MCP tool named `mcp__<server>__<tool>`.
fn mcp_summary(tool: &str) -> Option<String> {
    let (server, name) = tool.strip_prefix("mcp__")?.split_once("__")?;
    Some(format!("{server}/{name}"))
}

/// The questions of an `AskUserQuestion` input with their option labels,
/// in order. Questions without text are skipped.
pub(super) fn questions(input: &Value) -> Vec<(String, Vec<String>)> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|question| {
            let text = question.get("question").and_then(Value::as_str)?;
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    option
                        .get("label")
                        .and_then(Value::as_str)
                        .or_else(|| option.as_str())
                })
                .map(str::to_string)
                .collect();
            Some((text.to_string(), options))
        })
        .collect()
}
