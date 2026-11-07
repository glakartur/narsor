//! Which lines and tool calls of a Claude Code run belong to a subagent.
//!
//! A line written inside a subagent carries the id of the `Task`/`Agent`
//! call that started it in `parent_tool_use_id`; a main-agent line carries
//! `null`. A nested subagent's lines carry the id of its own, nearest call.

use std::collections::HashMap;

use narsor_protocol::chat::SubagentRef;
use serde_json::{Map, Value};

/// The name of a subagent whose call has neither a description nor a type.
const UNNAMED_SUBAGENT: &str = "subagent";

/// The tools that start a subagent.
const SUBAGENT_TOOLS: [&str; 2] = ["Task", "Agent"];

/// The id of the subagent call a line belongs to; `None` for the main agent.
pub(super) fn parent_call_id(line: &Map<String, Value>) -> Option<&str> {
    line.get("parent_tool_use_id").and_then(Value::as_str)
}

/// Remembers the tool calls of a run, to tell whose each line and denial is.
#[derive(Debug, Default)]
pub(super) struct SubagentTracker {
    /// Tool call id -> id of the subagent call it was made under.
    parent_of_call: HashMap<String, String>,
    /// Subagent call id -> the name to show.
    name_of_subagent: HashMap<String, String>,
    /// The subagent of the line seen last.
    last_line: Option<SubagentRef>,
}

impl SubagentTracker {
    /// Takes one parsed line: remembers the tool calls it makes and the
    /// subagent it belongs to.
    pub(super) fn observe(&mut self, line: &Map<String, Value>) {
        let parent = parent_call_id(line);
        if line.get("type").and_then(Value::as_str) == Some("assistant") {
            self.remember_calls(line, parent);
        }
        self.last_line = parent.map(|id| self.subagent(id));
    }

    /// The subagent of the last line observed.
    pub(super) fn last_line(&self) -> Option<SubagentRef> {
        self.last_line.clone()
    }

    /// The subagent that made the tool call `call_id`; `None` for the main
    /// agent and for a call never seen.
    pub(super) fn subagent_of_call(&self, call_id: &str) -> Option<SubagentRef> {
        self.parent_of_call.get(call_id).map(|id| self.subagent(id))
    }

    fn subagent(&self, id: &str) -> SubagentRef {
        SubagentRef {
            id: id.to_string(),
            name: self
                .name_of_subagent
                .get(id)
                .cloned()
                .unwrap_or_else(|| UNNAMED_SUBAGENT.to_string()),
        }
    }

    fn remember_calls(&mut self, line: &Map<String, Value>, parent: Option<&str>) {
        let blocks = line
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
        for block in blocks {
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(parent) = parent {
                self.parent_of_call
                    .insert(id.to_string(), parent.to_string());
            }
            let tool = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if SUBAGENT_TOOLS.contains(&tool) {
                let name = subagent_name(block.get("input"));
                self.name_of_subagent.insert(id.to_string(), name);
            }
        }
    }
}

/// The `description` of a subagent call, else its `subagent_type`, else a
/// fixed text.
fn subagent_name(input: Option<&Value>) -> String {
    let field = |name: &str| {
        input
            .and_then(|input| input.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    field("description")
        .or_else(|| field("subagent_type"))
        .unwrap_or(UNNAMED_SUBAGENT)
        .to_string()
}
