//! Classification of Claude Code `stream-json` lines into chat elements.
//!
//! Runs the `claude-code` stream parser over the real recordings in
//! `tests/fixtures/claude-code/` (see the README there) and over synthetic
//! lines for shapes the recordings do not contain.

use std::fs;
use std::path::PathBuf;

use narsor_engine::engine::actions::harness::HarnessStreamParser;
use narsor_engine::engine::{ClaudeCodeAdapter, HarnessAdapter};
use narsor_protocol::chat::{
    ChatElement, QuestionSource, TechnicalCategory, ToolResultStatus, UnclassifiedReason,
};
use serde_json::{json, Value};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code")
}

/// The lines of one recording.
fn recording(name: &str) -> Vec<String> {
    let path = fixtures_dir().join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

fn parser() -> Box<dyn HarnessStreamParser> {
    ClaudeCodeAdapter::new().stream_parser()
}

/// The elements of each line, from one fresh parser fed the lines in order.
fn classify<S: AsRef<str>>(lines: &[S]) -> Vec<Vec<ChatElement>> {
    let mut parser = parser();
    lines
        .iter()
        .map(|line| parser.on_stdout_line(line.as_ref().as_bytes()))
        .collect()
}

/// All elements of a run, flattened in order.
fn classify_all<S: AsRef<str>>(lines: &[S]) -> Vec<ChatElement> {
    classify(lines).into_iter().flatten().collect()
}

/// The elements of one line on a fresh parser.
fn classify_one(line: &str) -> Vec<ChatElement> {
    parser().on_stdout_line(line.as_bytes())
}

fn assistant(content: Value) -> String {
    json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": content},
        "session_id": "s-1",
    })
    .to_string()
}

fn user(content: Value) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "session_id": "s-1",
    })
    .to_string()
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn tool_calls(elements: &[ChatElement]) -> Vec<(Option<String>, String, String)> {
    elements
        .iter()
        .filter_map(|element| match element {
            ChatElement::ToolCall {
                call_id,
                tool,
                summary,
                ..
            } => Some((call_id.clone(), tool.clone(), summary.clone())),
            _ => None,
        })
        .collect()
}

/// The `summary` field of the single tool call `name` called with `input`.
fn call_summary(name: &str, input: Value) -> String {
    match classify_one(&assistant(json!([tool_use("toolu_1", name, input)]))).as_slice() {
        [ChatElement::ToolCall { tool, summary, .. }] => {
            assert_eq!(tool, name);
            summary.clone()
        }
        other => panic!("expected one tool call for {name}, got {other:?}"),
    }
}

#[test]
fn every_recording_classifies_every_line() {
    let mut names: Vec<_> = fs::read_dir(fixtures_dir())
        .expect("fixtures directory")
        .map(|entry| entry.expect("entry").file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".ndjson"))
        .collect();
    names.sort();
    assert!(names.len() >= 10, "{names:?}");

    for name in names {
        let lines = recording(&name);
        for (index, elements) in classify(&lines).into_iter().enumerate() {
            assert!(!elements.is_empty(), "{name}:{} gave no element", index + 1);
            for element in elements {
                assert!(
                    !matches!(
                        element,
                        ChatElement::Unclassified { .. } | ChatElement::Unrecognized
                    ),
                    "{name}:{} is unclassified: {element:?}",
                    index + 1
                );
            }
        }
    }
}

// Covers AE1.
#[test]
fn a_skill_call_names_the_skill() {
    let elements = classify_all(&recording("tools.ndjson"));
    let skill = elements
        .iter()
        .find(|element| matches!(element, ChatElement::ToolCall { tool, .. } if tool == "Skill"))
        .expect("a Skill call in tools.ndjson");
    let summary = skill.summary();
    assert!(summary.contains("Skill"), "{summary}");
    assert!(summary.contains("hello"), "{summary}");

    let synthetic = call_summary("Skill", json!({"skill": "ce-plan", "args": "x"}));
    assert_eq!(synthetic, "ce-plan");
}

// Covers AE2.
#[test]
fn a_failed_bash_result_is_tied_to_its_call() {
    let elements = classify_all(&recording("tool_error.ndjson"));
    let (call_id, _, summary) = tool_calls(&elements)
        .into_iter()
        .find(|(_, tool, _)| tool == "Bash")
        .expect("a Bash call");
    assert_eq!(summary, "ls /definitely/missing/dir");

    let result = elements
        .iter()
        .find(|element| {
            matches!(element, ChatElement::ToolResult { call_id: id, .. } if *id == call_id)
        })
        .expect("the Bash result");
    match result {
        ChatElement::ToolResult {
            tool,
            call_summary,
            status,
            content,
            truncated,
            ..
        } => {
            assert_eq!(tool.as_deref(), Some("Bash"));
            assert_eq!(call_summary.as_deref(), Some("ls /definitely/missing/dir"));
            assert_eq!(*status, ToolResultStatus::Error);
            assert!(content.contains("was blocked"), "{content}");
            assert!(!truncated);
        }
        _ => unreachable!(),
    }

    // The Read failure in the same run, too.
    assert!(elements.iter().any(|element| matches!(
        element,
        ChatElement::ToolResult { tool: Some(tool), call_summary: Some(path), status: ToolResultStatus::Error, .. }
            if tool == "Read" && path == "/workspace/project/missing.txt"
    )));
}

// Covers AE4.
#[test]
fn ask_user_question_is_an_agent_question() {
    let line = assistant(json!([tool_use(
        "toolu_q",
        "AskUserQuestion",
        json!({"questions": [{
            "question": "Red or blue?",
            "header": "Colour",
            "multiSelect": false,
            "options": [
                {"label": "red", "description": "warm"},
                {"label": "blue", "description": "cold"}
            ]
        }]})
    )]));
    assert_eq!(
        classify_one(&line),
        [ChatElement::AgentQuestion {
            question: "Red or blue?".into(),
            options: vec!["red".into(), "blue".into()],
            source: QuestionSource::Tool,
            call_id: Some("toolu_q".into()),
        }]
    );

    // Its answer is an ordinary tool result tied to the question.
    let answer = user(json!([{
        "type": "tool_result", "tool_use_id": "toolu_q", "content": "red"
    }]));
    let elements = classify(&[line, answer]);
    match elements[1].as_slice() {
        [ChatElement::ToolResult {
            tool,
            call_summary,
            status,
            ..
        }] => {
            assert_eq!(tool.as_deref(), Some("AskUserQuestion"));
            assert_eq!(call_summary.as_deref(), Some("Red or blue?"));
            assert_eq!(*status, ToolResultStatus::Ok);
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[test]
fn several_ask_user_questions_are_joined() {
    let line = assistant(json!([tool_use(
        "toolu_q",
        "AskUserQuestion",
        json!({"questions": [
            {"question": "Red or blue?", "options": [{"label": "red"}, {"label": "blue"}]},
            {"question": "How many?", "options": [{"label": "1"}, {"label": "2"}]}
        ]})
    )]));
    match classify_one(&line).as_slice() {
        [ChatElement::AgentQuestion {
            question,
            options,
            source: QuestionSource::Tool,
            ..
        }] => {
            assert!(question.contains("Red or blue? [red | blue]"), "{question}");
            assert!(question.contains("How many? [1 | 2]"), "{question}");
            assert!(options.is_empty(), "{options:?}");
        }
        other => panic!("expected an agent question, got {other:?}"),
    }
}

// Covers AE6.
#[test]
fn an_unknown_type_does_not_stop_classification() {
    let elements = classify(&[
        json!({"type": "brand_new_event", "x": 1}).to_string(),
        assistant(json!([{"type": "text", "text": "still here"}])),
    ]);
    assert_eq!(
        elements[0],
        [ChatElement::Unclassified {
            reason: UnclassifiedReason::UnknownType
        }]
    );
    assert_eq!(
        elements[1],
        [ChatElement::AgentText {
            text: "still here".into()
        }]
    );
}

#[test]
fn an_unknown_content_block_is_unclassified() {
    let elements = classify_one(&assistant(json!([
        {"type": "text", "text": "a"},
        {"type": "hologram", "data": 1}
    ])));
    assert_eq!(
        elements,
        [
            ChatElement::AgentText { text: "a".into() },
            ChatElement::Unclassified {
                reason: UnclassifiedReason::UnknownBlock
            }
        ]
    );
}

// Covers AE7.
#[test]
fn init_starts_the_session_and_rate_limits_are_technical() {
    let elements = classify_all(&recording("tools.ndjson"));
    let started = elements
        .iter()
        .find(|element| matches!(element, ChatElement::SessionStarted { .. }))
        .expect("session started");
    assert_eq!(
        *started,
        ChatElement::SessionStarted {
            session_id: Some("00000000-0000-4000-8000-000000000205".into()),
            model: Some("claude-sonnet-5-5".into()),
            cli_version: Some("2.1.302".into()),
            cwd: Some("/workspace/project".into()),
            permission_mode: Some("acceptEdits".into()),
        }
    );

    let rate_limit = elements
        .iter()
        .find_map(|element| match element {
            ChatElement::Technical {
                category: TechnicalCategory::RateLimit,
                summary,
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .expect("a rate limit element");
    assert!(rate_limit.contains("allowed"), "{rate_limit}");
    assert!(rate_limit.contains("five_hour"), "{rate_limit}");
}

#[test]
fn hooks_are_technical_with_their_name_and_exit_code() {
    let summaries: Vec<String> = classify_all(&recording("tools.ndjson"))
        .into_iter()
        .filter_map(|element| match element {
            ChatElement::Technical {
                category: TechnicalCategory::Hook,
                summary,
                ..
            } => Some(summary),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.len(), 6, "{summaries:?}");
    assert!(summaries.contains(&"SessionStart:startup started".to_string()));
    assert!(summaries.contains(&"SessionStart:startup exit 0".to_string()));
    assert!(
        summaries
            .iter()
            .any(|summary| summary.starts_with("SessionStart:startup exit 127")),
        "{summaries:?}"
    );
}

#[test]
fn other_system_lines_are_technical_system() {
    let elements = classify_all(&recording("tool_error.ndjson"));
    let system: Vec<String> = elements
        .iter()
        .filter_map(|element| match element {
            ChatElement::Technical {
                category: TechnicalCategory::System,
                summary,
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .collect();
    assert!(
        system
            .iter()
            .any(|summary| summary.contains("permission denied")
                && summary.contains("Bash")
                && summary.contains("was blocked")),
        "{system:?}"
    );
    assert!(
        system.iter().any(|summary| summary.contains("thinking")),
        "{system:?}"
    );
}

#[test]
fn compaction_is_technical_compaction() {
    let line = json!({
        "type": "system",
        "subtype": "compact_boundary",
        "compact_metadata": {"trigger": "auto", "pre_tokens": 155000},
        "session_id": "s-1"
    })
    .to_string();
    match classify_one(&line).as_slice() {
        [ChatElement::Technical {
            category: TechnicalCategory::Compaction,
            summary,
            ..
        }] => {
            assert!(summary.contains("auto"), "{summary}");
            assert!(summary.contains("155000"), "{summary}");
        }
        other => panic!("expected a compaction element, got {other:?}"),
    }
}

#[test]
fn text_and_two_tool_calls_keep_their_order() {
    let elements = classify_one(&assistant(json!([
        {"type": "text", "text": "Let me look."},
        tool_use("toolu_a", "Read", json!({"file_path": "/w/a.rs"})),
        tool_use("toolu_b", "Bash", json!({"command": "cargo test", "description": "Run tests"}))
    ])));
    assert_eq!(elements.len(), 3, "{elements:?}");
    assert_eq!(
        elements[0],
        ChatElement::AgentText {
            text: "Let me look.".into()
        }
    );
    assert_eq!(
        tool_calls(&elements[1..]),
        [
            (Some("toolu_a".into()), "Read".into(), "/w/a.rs".into()),
            (Some("toolu_b".into()), "Bash".into(), "cargo test".into()),
        ]
    );
    match &elements[2] {
        ChatElement::ToolCall { input, .. } => {
            assert_eq!(input["description"], "Run tests");
        }
        _ => unreachable!(),
    }
}

#[test]
fn thinking_without_text_is_redacted() {
    let elements = classify_all(&recording("thinking.ndjson"));
    assert!(
        elements.contains(&ChatElement::Thinking {
            text: String::new(),
            redacted: true
        }),
        "{elements:?}"
    );

    assert_eq!(
        classify_one(&assistant(json!([
            {"type": "thinking", "thinking": "Hmm, let me see.", "signature": "s"},
            {"type": "redacted_thinking", "data": "opaque"}
        ]))),
        [
            ChatElement::Thinking {
                text: "Hmm, let me see.".into(),
                redacted: false
            },
            ChatElement::Thinking {
                text: String::new(),
                redacted: true
            }
        ]
    );
}

#[test]
fn user_text_is_a_user_message() {
    // The skill body Claude Code injects after a Skill call.
    let elements = classify_all(&recording("tools.ndjson"));
    assert!(elements.iter().any(|element| matches!(
        element,
        ChatElement::UserMessage { text } if text.starts_with("Base directory for this skill")
    )));

    assert_eq!(
        classify_one(&user(json!("plain string content"))),
        [ChatElement::UserMessage {
            text: "plain string content".into()
        }]
    );
}

#[test]
fn a_successful_result_carries_the_run_metadata() {
    let elements = classify_all(&recording("tools.ndjson"));
    let last = elements.last().expect("elements");
    match last {
        ChatElement::RunResult {
            text,
            structured_output,
            stop_reason,
            is_error,
            cost_usd,
            usage,
            turns,
            duration_ms,
        } => {
            assert!(text
                .as_deref()
                .is_some_and(|text| text.starts_with("Hello from the hello skill.")));
            assert_eq!(*structured_output, None);
            assert_eq!(stop_reason.as_deref(), Some("end_turn"));
            assert!(!is_error);
            assert!(cost_usd.is_some_and(|cost| cost > 0.0));
            assert!(usage
                .as_ref()
                .is_some_and(|usage| usage["output_tokens"].is_u64()));
            assert_eq!(*turns, Some(6));
            assert_eq!(*duration_ms, Some(8784));
        }
        other => panic!("expected a run result last, got {other:?}"),
    }
}

#[test]
fn max_turns_is_an_error_result_with_its_reason() {
    let elements = classify_all(&recording("max_turns.ndjson"));
    match elements.last().expect("elements") {
        ChatElement::RunResult {
            text,
            stop_reason,
            is_error,
            turns,
            ..
        } => {
            assert!(is_error);
            assert_eq!(stop_reason.as_deref(), Some("error_max_turns"));
            assert_eq!(text.as_deref(), Some("Reached maximum number of turns (1)"));
            assert_eq!(*turns, Some(2));
        }
        other => panic!("expected a run result, got {other:?}"),
    }
}

#[test]
fn an_api_error_reports_its_terminal_reason() {
    let line = json!({
        "type": "result", "subtype": "success", "is_error": true,
        "stop_reason": null, "terminal_reason": "api_error",
        "result": "API Error: 400", "num_turns": 1, "duration_ms": 10,
        "total_cost_usd": 0.0, "session_id": "s-1"
    })
    .to_string();
    match classify_one(&line).as_slice() {
        [ChatElement::RunResult {
            stop_reason,
            is_error,
            ..
        }] => {
            assert!(is_error);
            assert_eq!(stop_reason.as_deref(), Some("api_error"));
        }
        other => panic!("expected a run result, got {other:?}"),
    }
}

#[test]
fn awaiting_input_structured_output_is_only_a_run_result() {
    // The question is the executor's to record, from the answer it accepts.
    let lines = recording("schema_awaiting.ndjson");
    let elements = classify(&lines);
    let last = elements.last().expect("lines");
    match last.as_slice() {
        [ChatElement::RunResult {
            structured_output: Some(output),
            is_error: false,
            ..
        }] => {
            assert_eq!(output["status"], "awaiting_input");
            assert!(output["question"]
                .as_str()
                .is_some_and(|question| question.starts_with("Which file do you want to rename?")));
        }
        other => panic!("expected only a run result, got {other:?}"),
    }

    // A done structured output is only a run result.
    let done = classify(&recording("schema_done.ndjson"));
    assert!(matches!(
        done.last().expect("lines").as_slice(),
        [ChatElement::RunResult {
            structured_output: Some(_),
            ..
        }]
    ));
}

#[test]
fn structured_output_calls_summarise_the_answer() {
    let summaries: Vec<String> = tool_calls(&classify_all(&recording("schema_mismatch.ndjson")))
        .into_iter()
        .filter(|(_, tool, _)| tool == "StructuredOutput")
        .map(|(_, _, summary)| summary)
        .collect();
    assert_eq!(summaries, ["choice: finished", "choice: done"]);

    let awaiting = tool_calls(&classify_all(&recording("schema_awaiting.ndjson")))
        .into_iter()
        .find(|(_, tool, _)| tool == "StructuredOutput")
        .expect("a StructuredOutput call")
        .2;
    assert!(
        awaiting.starts_with("awaiting_input: Which file"),
        "{awaiting}"
    );
}

#[test]
fn an_oversized_tool_result_is_truncated() {
    let big = "ż".repeat(10_000); // 20 000 bytes, two per char
    let elements = classify(&[
        assistant(json!([tool_use(
            "toolu_r",
            "Read",
            json!({"file_path": "/big"})
        )])),
        user(json!([{"type": "tool_result", "tool_use_id": "toolu_r", "content": big}])),
    ]);
    match elements[1].as_slice() {
        [ChatElement::ToolResult {
            content, truncated, ..
        }] => {
            assert!(truncated);
            assert!(content.len() < big.len());
            assert!(content.len() >= 1024, "{}", content.len());
            assert!(big.starts_with(content.as_str()));
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[test]
fn tool_result_text_blocks_are_joined() {
    let elements = classify_one(&user(json!([{
        "type": "tool_result",
        "tool_use_id": "toolu_x",
        "content": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}]
    }])));
    match elements.as_slice() {
        [ChatElement::ToolResult {
            content, truncated, ..
        }] => {
            assert_eq!(content, "one\ntwo");
            assert!(!truncated);
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[test]
fn a_result_for_an_unknown_call_has_no_tool() {
    assert_eq!(
        classify_one(&user(json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_gone",
            "content": "ok",
            "is_error": false
        }]))),
        [ChatElement::ToolResult {
            call_id: Some("toolu_gone".into()),
            tool: None,
            call_summary: None,
            status: ToolResultStatus::Ok,
            content: "ok".into(),
            truncated: false,
        }]
    );
}

#[test]
fn lines_that_are_not_json_objects_are_invalid_json() {
    let invalid = vec![ChatElement::Unclassified {
        reason: UnclassifiedReason::InvalidJson,
    }];
    for line in ["not json", "[1, 2]", "", "42"] {
        assert_eq!(classify_one(line), invalid, "{line:?}");
    }
    assert_eq!(parser().on_stdout_line(&[0xff, 0xfe, b'{']), invalid);
}

#[test]
fn tool_calls_are_summarised_by_their_key_parameter() {
    let cases = [
        (
            "Bash",
            json!({"command": "ls -la\n  && pwd", "description": "List"}),
            "ls -la && pwd",
        ),
        ("Read", json!({"file_path": "/w/a.rs"}), "/w/a.rs"),
        (
            "Write",
            json!({"file_path": "/w/b.rs", "content": "x"}),
            "/w/b.rs",
        ),
        (
            "Edit",
            json!({"file_path": "/w/c.rs", "old_string": "a", "new_string": "b"}),
            "/w/c.rs",
        ),
        (
            "NotebookEdit",
            json!({"notebook_path": "/w/n.ipynb"}),
            "/w/n.ipynb",
        ),
        ("Glob", json!({"pattern": "**/*.rs"}), "**/*.rs"),
        (
            "Grep",
            json!({"pattern": "needle", "path": "src"}),
            "needle in src",
        ),
        (
            "WebFetch",
            json!({"url": "https://example.com", "prompt": "p"}),
            "https://example.com",
        ),
        ("WebSearch", json!({"query": "rust serde"}), "rust serde"),
        ("Skill", json!({"skill": "hello"}), "hello"),
        (
            "Task",
            json!({"description": "Explore", "subagent_type": "Explore", "prompt": "p"}),
            "Explore (Explore)",
        ),
        (
            "Agent",
            json!({"description": "Review code", "prompt": "p"}),
            "Review code",
        ),
        (
            "TodoWrite",
            json!({"todos": [{"content": "a"}, {"content": "b"}, {"content": "c"}]}),
            "3 tasks",
        ),
        (
            "ToolSearch",
            json!({"query": "select:AskUserQuestion", "max_results": 1}),
            "select:AskUserQuestion",
        ),
        (
            "mcp__github__get_issue",
            json!({"number": 1}),
            "github/get_issue",
        ),
        ("SomethingNew", json!({"x": 1}), ""),
    ];
    for (tool, input, expected) in cases {
        assert_eq!(call_summary(tool, input), expected, "{tool}");
    }
}

#[test]
fn a_long_key_parameter_is_cut_to_one_bounded_line() {
    let summary = call_summary("Bash", json!({"command": "x".repeat(1000)}));
    assert!(summary.chars().count() <= narsor_protocol::chat::SUMMARY_MAX_CHARS);
    assert!(summary.ends_with('…'), "{summary}");
}

#[test]
fn the_harness_run_is_still_accumulated() {
    let mut parser = parser();
    for line in recording("schema_done.ndjson") {
        parser.on_stdout_line(line.as_bytes());
    }
    let run = parser.finish(0, "");
    assert_eq!(run.failure, None);
    assert_eq!(run.text, r#"{"choice":"done","additional_data":{"lines":3}}"#);
    assert_eq!(run.reply.as_deref(), Some("Plik `notes.txt` ma 3 linie (`wc -l` zwróciło 3). Treść: `alpha`, `beta`, `gamma needle`."));
    assert!(run.session_id.is_some());

    let mut parser = self::parser();
    for line in recording("max_turns.ndjson") {
        parser.on_stdout_line(line.as_bytes());
    }
    let run = parser.finish(1, "");
    assert!(run.failure.is_some());
}

// ---------------------------------------------------------------------------
// The structured output of a `--json-schema` run (U5)
// ---------------------------------------------------------------------------

/// Feeds a whole recording to one parser and distills its run.
fn run_of(name: &str) -> narsor_engine::engine::actions::harness::HarnessRun {
    let mut parser = parser();
    for line in recording(name) {
        parser.on_stdout_line(line.as_bytes());
    }
    parser.finish(0, "")
}

#[test]
fn a_schema_run_reports_its_structured_output() {
    let run = run_of("schema_done.ndjson");

    assert_eq!(run.failure, None);
    let output = run.structured_output.expect("a structured output");
    assert_eq!(output["choice"], "done");
    assert!(output["additional_data"].is_object(), "{output}");
}

#[test]
fn a_schema_question_is_reported_as_structured_output_too() {
    let run = run_of("schema_awaiting.ndjson");

    let output = run.structured_output.expect("a structured output");
    assert_eq!(output["status"], "awaiting_input");
    assert!(output["question"].as_str().is_some_and(|q| !q.is_empty()));
}

#[test]
fn a_run_without_a_schema_has_no_structured_output() {
    assert_eq!(run_of("tools.ndjson").structured_output, None);
}

#[test]
fn a_result_preserves_structured_permission_denials_and_correlation() {
    let mut parser = parser();
    let denied = json!({
        "type": "system",
        "subtype": "permission_denied",
        "tool_name": "Bash",
        "tool_use_id": "tool-1",
        "decision_reason_type": "subcommandResults",
        "message": "Approval is required for this command."
    })
    .to_string();
    parser.on_stdout_line(denied.as_bytes());
    let line = json!({
        "type": "result",
        "subtype": "success",
        "session_id": "session-1",
        "permission_denials": [{
            "tool_name": "Bash",
            "tool_use_id": "tool-1",
            "tool_input": {"command": "cargo build"}
        }],
        "result": "The command was blocked.",
        "is_error": false
    })
    .to_string();
    let elements = parser.on_stdout_line(line.as_bytes());
    let run = parser.finish(0, "");

    assert_eq!(run.session_id.as_deref(), Some("session-1"));
    assert_eq!(run.permission_denials.len(), 1);
    let denial = &run.permission_denials[0];
    assert_eq!(denial.tool_name, "Bash");
    assert_eq!(denial.tool_use_id, "tool-1");
    assert_eq!(denial.input["command"], "cargo build");
    assert_eq!(denial.reason_type.as_deref(), Some("subcommandResults"));
    assert_eq!(
        denial.reason.as_deref(),
        Some("Approval is required for this command.")
    );
    assert!(run
        .failure
        .as_deref()
        .is_some_and(|failure| failure.contains("reported an error")));
    assert!(matches!(
        elements.last(),
        Some(ChatElement::RunResult { is_error: true, .. })
    ));
}

/// The run of `lines`, ending with a `result` line carrying `result`.
fn run_of_lines(
    lines: &[String],
    result: &str,
) -> narsor_engine::engine::actions::harness::HarnessRun {
    let mut parser = parser();
    for line in lines {
        parser.on_stdout_line(line.as_bytes());
    }
    let end = json!({
        "type": "result",
        "subtype": "success",
        "session_id": "s-1",
        "result": result,
        "is_error": false
    })
    .to_string();
    parser.on_stdout_line(end.as_bytes());
    parser.finish(0, "")
}

#[test]
fn a_subagents_messages_are_not_the_reply() {
    let subagent = json!({
        "type": "assistant",
        "parent_tool_use_id": "task-1",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "subagent notes"}]},
        "session_id": "s-1",
    })
    .to_string();
    let run = run_of_lines(
        &[
            subagent,
            assistant(json!([{"type": "text", "text": "The final report."}])),
        ],
        r#"{"choice":"done"}"#,
    );

    assert_eq!(run.reply.as_deref(), Some("The final report."));
}

#[test]
fn only_the_prose_after_the_last_tool_call_is_the_reply() {
    let run = run_of_lines(
        &[
            assistant(json!([
                {"type": "text", "text": "Let me run the tests first."},
                {"type": "tool_use", "id": "tool-1", "name": "Bash", "input": {"command": "cargo test"}},
            ])),
            assistant(json!([{"type": "text", "text": "All green."}])),
        ],
        r#"{"choice":"done"}"#,
    );

    assert_eq!(run.reply.as_deref(), Some("All green."));
}
