//! Integration tests for the per-attempt conversation store (U2).
//!
//! Every test runs against a real temporary directory: the whole value of
//! this layer lies in real file operations (append, compress, rename), so
//! nothing here is mocked.

use chrono::Utc;
use narsor_engine::domain::{
    Context, EventData, ExecutionStatus, RepositoryInfo, WorkflowExecution, Workspace,
};
use narsor_engine::persistence::conversation::ConversationSource;
use narsor_engine::persistence::recovery::compress_closed_conversations;
use narsor_engine::persistence::ExecutionStore;
use narsor_protocol::chat::{ChatElement, PromptPurpose, TechnicalCategory, UnclassifiedReason};
use std::fs;
use std::path::PathBuf;

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this
/// crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("narsor-conversations-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn store(&self) -> ExecutionStore {
        ExecutionStore::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn sample_execution(id: &str, status: ExecutionStatus) -> WorkflowExecution {
    WorkflowExecution {
        id: id.to_string(),
        workflow_id: "implement-feature".to_string(),
        workflow_version: 1,
        status,
        current_state: Some("plan".to_string()),
        started_at: Utc::now(),
        completed_at: None,
        inputs: serde_json::json!({}),
        context: Context::new(),
        workspace: Workspace::git_checkout(
            "/tmp/workspace".to_string(),
            RepositoryInfo {
                id: "repo-1".to_string(),
                name: "narsor".to_string(),
                provider: "gitlab".to_string(),
                remote: "git@example.com:team/narsor.git".to_string(),
            },
            Some("main".to_string()),
        ),
        current_action_execution: None,
        metadata: serde_json::json!({}),
    }
}

fn action_started(action_execution_id: &str) -> EventData {
    EventData::ActionStarted {
        action_execution_id: action_execution_id.to_string(),
        state_id: "plan".to_string(),
        action_type: "agent".to_string(),
    }
}

fn action_completed(action_execution_id: &str) -> EventData {
    EventData::ActionCompleted {
        action_execution_id: action_execution_id.to_string(),
        output_label: "success".to_string(),
    }
}

fn action_attempt_failed(action_execution_id: &str) -> EventData {
    EventData::ActionAttemptFailed {
        action_execution_id: action_execution_id.to_string(),
        state_id: "plan".to_string(),
        error: "boom".to_string(),
        retry_in_ms: 100,
    }
}

// --- Happy paths -----------------------------------------------------------

#[test]
fn writing_three_entries_and_closing_leaves_only_the_compressed_file() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-1");

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append_narsor("prompt: do the thing", &[])
        .expect("append prompt");
    writer
        .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#, &[])
        .expect("append stdout");
    writer
        .append_narsor("exit code: 0", &[])
        .expect("append completion");
    writer.finish().expect("finish writer");

    let jsonl_path = conversations.dir().join("attempt-1.jsonl");
    assert!(jsonl_path.exists(), "plain file must exist before closing");

    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");

    let zst_path = conversations.dir().join("attempt-1.jsonl.zst");
    assert!(
        zst_path.exists(),
        "compressed file must exist after closing"
    );
    assert!(
        !jsonl_path.exists(),
        "plain file must be gone after closing"
    );

    let snapshot = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("attempt must have a record");

    assert!(snapshot.closed);
    assert!(!snapshot.truncated_tail);
    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.entries[0].entry_number, 1);
    assert_eq!(snapshot.entries[0].raw, b"prompt: do the thing");
    assert_eq!(snapshot.entries[1].entry_number, 2);
    assert_eq!(snapshot.entries[1].raw, br#"{"type":"assistant"}"#);
    assert_eq!(snapshot.entries[2].entry_number, 3);
    assert_eq!(snapshot.entries[2].raw, b"exit code: 0");
    assert_eq!(snapshot.next_entry, 4);
}

#[test]
fn elements_are_stored_with_their_raw_line_and_survive_compression() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");

    let prompt = vec![ChatElement::Prompt {
        text: "do the thing".to_string(),
        resumed_session: Some("session-1".to_string()),
        purpose: PromptPurpose::Step,
    }];
    let stdout = vec![
        ChatElement::AgentText {
            text: "on it".to_string(),
        },
        ChatElement::Unclassified {
            reason: UnclassifiedReason::UnknownBlock,
        },
    ];
    let raw_stdout: &[u8] = b"{\"type\":\"assistant\"}\xff\r";

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append_narsor("prompt: do the thing", &prompt)
        .expect("append prompt");
    writer
        .append(ConversationSource::Stdout, raw_stdout, &stdout)
        .expect("append stdout");
    writer.finish().expect("finish writer");

    let open = conversations
        .read("attempt-1", 1, usize::MAX)
        .expect("read")
        .expect("a record");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");
    let closed = conversations
        .read("attempt-1", 1, usize::MAX)
        .expect("read")
        .expect("a record");

    for snapshot in [open, closed] {
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].raw, b"prompt: do the thing");
        assert_eq!(snapshot.entries[0].elements, prompt);
        assert_eq!(snapshot.entries[1].raw, raw_stdout, "byte for byte");
        assert_eq!(snapshot.entries[1].elements, stdout);
    }
}

#[test]
fn an_entry_without_elements_is_written_without_the_field() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append(ConversationSource::Stdout, b"plain", &[])
        .expect("append");
    writer.finish().expect("finish writer");

    let line = fs::read_to_string(conversations.dir().join("attempt-1.jsonl")).expect("read");
    let envelope: serde_json::Value = serde_json::from_str(line.trim()).expect("an envelope");
    assert!(envelope.get("elements").is_none(), "{line}");
}

#[test]
fn an_envelope_written_before_elements_existed_reads_with_none() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");
    fs::create_dir_all(conversations.dir()).expect("create the directory");
    // The exact envelope shape every record had before elements existed.
    fs::write(
        conversations.dir().join("attempt-old.jsonl"),
        concat!(
            r#"{"n":1,"ts":"2026-01-02T03:04:05Z","source":"narsor","raw_b64":"cHJvbXB0"}"#,
            "\n",
            r#"{"n":2,"ts":"2026-01-02T03:04:06Z","source":"stdout","raw_b64":"e30="}"#,
            "\n",
        ),
    )
    .expect("write an old record");

    let snapshot = conversations
        .read("attempt-old", 1, usize::MAX)
        .expect("an old record is not an error")
        .expect("a record");
    assert_eq!(snapshot.entries.len(), 2);
    assert_eq!(snapshot.entries[0].raw, b"prompt");
    assert_eq!(snapshot.entries[1].raw, b"{}");
    assert!(snapshot
        .entries
        .iter()
        .all(|entry| entry.elements.is_empty()));
    assert_eq!(snapshot.next_entry, 3);
}

#[test]
fn an_element_that_cannot_be_read_back_stands_alone_as_unreadable() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");
    fs::create_dir_all(conversations.dir()).expect("create the directory");
    // Entry 2 holds a known kind without its required field, and one whose
    // field has another type, between two elements that read fine.
    fs::write(
        conversations.dir().join("attempt-1.jsonl"),
        concat!(
            r#"{"n":1,"ts":"2026-01-02T03:04:05Z","source":"narsor","raw_b64":"cHJvbXB0","elements":[{"kind":"agent_text","text":"first"}]}"#,
            "\n",
            r#"{"n":2,"ts":"2026-01-02T03:04:06Z","source":"stdout","raw_b64":"e30=","elements":[{"kind":"agent_text","text":"before"},{"kind":"agent_text"},{"kind":"run_result","turns":"many"},{"kind":"agent_text","text":"after"}]}"#,
            "\n",
            r#"{"n":3,"ts":"2026-01-02T03:04:07Z","source":"stdout","raw_b64":"bGFzdA==","elements":[{"kind":"agent_text","text":"last"}]}"#,
            "\n",
        ),
    )
    .expect("write a record");

    let snapshot = conversations
        .read("attempt-1", 1, usize::MAX)
        .expect("one unreadable element is not an error")
        .expect("a record");

    let text = |text: &str| ChatElement::AgentText {
        text: text.to_string(),
    };
    let unreadable = ChatElement::Unclassified {
        reason: UnclassifiedReason::Unreadable,
    };
    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.entries[0].elements, vec![text("first")]);
    assert_eq!(snapshot.entries[1].raw, b"{}");
    assert_eq!(snapshot.entries[1].source, ConversationSource::Stdout);
    assert_eq!(
        snapshot.entries[1].elements,
        vec![
            text("before"),
            unreadable.clone(),
            unreadable,
            text("after")
        ]
    );
    assert_eq!(snapshot.entries[2].raw, b"last");
    assert_eq!(snapshot.entries[2].elements, vec![text("last")]);
    assert_eq!(snapshot.next_entry, 4);
}

#[test]
fn a_structured_output_nested_past_the_parse_limit_is_stored_and_reads() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");

    // serde_json refuses to parse more than 128 levels; writing has no
    // such limit, so the element is stored and only reading it back fails.
    let mut deep = serde_json::json!("bottom");
    for _ in 0..200 {
        deep = serde_json::json!([deep]);
    }
    let elements = vec![
        ChatElement::AgentText {
            text: "done".to_string(),
        },
        ChatElement::RunResult {
            text: None,
            structured_output: Some(deep),
            stop_reason: None,
            is_error: false,
            cost_usd: None,
            usage: None,
            turns: None,
            duration_ms: None,
        },
    ];
    let raw: &[u8] = b"{\"type\":\"result\"}";

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append(ConversationSource::Stdout, raw, &elements)
        .expect("a deep element is written");
    writer
        .append(ConversationSource::Stdout, b"next", &[])
        .expect("append next");
    writer.finish().expect("finish writer");

    let open = conversations
        .read("attempt-1", 1, usize::MAX)
        .expect("a deep element is not an error")
        .expect("a record");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");
    let closed = conversations
        .read("attempt-1", 1, usize::MAX)
        .expect("a deep element is not an error once closed")
        .expect("a record");

    for snapshot in [open, closed] {
        assert_eq!(snapshot.entries.len(), 2);
        assert_eq!(snapshot.entries[0].raw, raw, "byte for byte");
        assert_eq!(
            snapshot.entries[0].elements,
            vec![
                elements[0].clone(),
                ChatElement::Unclassified {
                    reason: UnclassifiedReason::Unreadable,
                },
            ]
        );
        assert_eq!(snapshot.entries[1].raw, b"next");
        assert_eq!(snapshot.next_entry, 3);
    }
}

#[test]
fn the_byte_budget_counts_the_elements_too() {
    let root = TempRoot::new();
    let conversations = root.store().conversation_store("exec-1");

    // Tiny raw lines, bulky elements: the raw bytes alone fit the budget
    // three times over, the elements do not.
    let bulky = |text: &str| {
        vec![ChatElement::Technical {
            category: TechnicalCategory::System,
            summary: text.repeat(200),
            data: None,
        }]
    };
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    for (raw, text) in [(b"a", "x"), (b"b", "y"), (b"c", "z")] {
        writer
            .append(ConversationSource::Stdout, raw, &bulky(text))
            .expect("append");
    }
    writer.finish().expect("finish writer");

    let first = conversations
        .read("attempt-1", 1, 100)
        .expect("read")
        .expect("a record");
    assert_eq!(
        first.entries.len(),
        1,
        "the page ends after the first entry"
    );
    assert_eq!(first.next_entry, 2, "the cursor points at the next entry");

    let rest = conversations
        .read("attempt-1", first.next_entry, 100)
        .expect("read")
        .expect("a record");
    assert_eq!(rest.entries[0].entry_number, 2);
    assert_eq!(rest.entries[0].raw, b"b");
    assert_eq!(rest.entries[0].elements, bulky("y"));

    let tiny = conversations
        .read("attempt-1", 1, 10)
        .expect("read")
        .expect("a record");
    assert_eq!(
        tiny.entries.len(),
        1,
        "a budget smaller than one entry still returns that entry whole"
    );
    let all = conversations
        .read("attempt-1", 1, 3 * 1024)
        .expect("read")
        .expect("a record");
    assert_eq!(all.entries.len(), 3, "a budget that fits all of it");
}

#[test]
fn reading_an_open_attempt_from_a_cursor_returns_only_new_entries() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-2");

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("one", &[]).expect("append 1");
    writer.append_narsor("two", &[]).expect("append 2");

    let first_read = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(first_read.entries.len(), 2);
    assert!(!first_read.closed);
    let cursor = first_read.next_entry;
    assert_eq!(cursor, 3);

    writer.append_narsor("three", &[]).expect("append 3");
    writer.append_narsor("four", &[]).expect("append 4");

    let second_read = conversations
        .read("attempt-1", cursor, 1 << 20)
        .expect("read")
        .expect("record must exist");

    assert_eq!(second_read.entries.len(), 2);
    assert_eq!(second_read.entries[0].entry_number, 3);
    assert_eq!(second_read.entries[0].raw, b"three");
    assert_eq!(second_read.entries[1].entry_number, 4);
    assert_eq!(second_read.entries[1].raw, b"four");
    assert!(!second_read.closed);
}

// --- Edge cases --------------------------------------------------------------

#[test]
fn a_truncated_last_line_is_omitted_while_open_and_reported_incomplete_once_closed() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-3");

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("complete entry", &[]).expect("append");
    writer.finish().expect("finish writer");

    // Simulate a writer killed mid-`write_all`: append raw, non-newline
    // terminated garbage directly to the file.
    let jsonl_path = conversations.dir().join("attempt-1.jsonl");
    let mut existing = fs::read(&jsonl_path).expect("read jsonl");
    existing.extend_from_slice(br#"{"n":2,"ts":"2024-01-01T00:00:00Z","source":"stdout","raw_"#);
    fs::write(&jsonl_path, &existing).expect("write truncated tail");

    let while_open = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(
        while_open.entries.len(),
        1,
        "the truncated line must be omitted while the attempt is open"
    );
    assert!(while_open.truncated_tail);
    assert!(!while_open.closed);

    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");

    let after_close = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(
        after_close.entries.len(),
        1,
        "the truncated line is never a returned entry, closed or not"
    );
    assert!(
        after_close.truncated_tail,
        "the truncated line must be reported as incomplete once closed"
    );
    assert!(after_close.closed);
}

#[test]
fn compression_between_two_reads_gives_a_correct_continuation() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-4");

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("one", &[]).expect("append 1");
    writer.append_narsor("two", &[]).expect("append 2");

    let first_read = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(first_read.entries.len(), 2);
    let cursor = first_read.next_entry;

    writer.append_narsor("three", &[]).expect("append 3");
    writer.finish().expect("finish writer");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");

    let second_read = conversations
        .read("attempt-1", cursor, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(second_read.entries.len(), 1);
    assert_eq!(second_read.entries[0].raw, b"three");
    assert!(second_read.closed);
}

#[test]
fn both_files_present_prefers_the_compressed_one_and_startup_deletes_the_plain_one() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-5", ExecutionStatus::Completed);
    store.save_execution(&execution).expect("save execution");

    let conversations = store.conversation_store("exec-5");
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("hello", &[]).expect("append");
    writer.finish().expect("finish writer");
    conversations
        .close_attempt("attempt-1")
        .expect("close attempt");

    // Simulate a crash between rename and delete: recreate the plain file
    // with different (stale) content next to the already-compressed one.
    let jsonl_path = conversations.dir().join("attempt-1.jsonl");
    fs::write(&jsonl_path, b"stale garbage, must never be read\n")
        .expect("recreate stale plain file");
    assert!(conversations.dir().join("attempt-1.jsonl.zst").exists());

    let snapshot = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert!(snapshot.closed);
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].raw, b"hello");

    compress_closed_conversations(&store).expect("startup compression sweep");
    assert!(
        !jsonl_path.exists(),
        "startup must delete the leftover plain file next to the compressed one"
    );
}

#[test]
fn raw_lines_that_are_not_json_or_not_utf8_are_preserved_exactly() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-6");

    let not_json: &[u8] = b"this is not json at all {[";
    // `foo` + two bytes that are not valid UTF-8 anywhere + `bar`.
    let invalid_utf8: &[u8] = &[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72];

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append(ConversationSource::Stdout, not_json, &[])
        .expect("append non-json line");
    writer
        .append(ConversationSource::Stdout, invalid_utf8, &[])
        .expect("append invalid utf-8 line");
    writer.finish().expect("finish writer");

    let snapshot = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(snapshot.entries.len(), 2);
    assert_eq!(snapshot.entries[0].raw, not_json);
    assert_eq!(snapshot.entries[1].raw, invalid_utf8);
}

#[test]
fn a_single_ten_mebibyte_entry_is_written_and_read_back_in_full() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-7");

    let big: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 251) as u8).collect();

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer
        .append(ConversationSource::Stdout, &big, &[])
        .expect("append big entry");
    writer.finish().expect("finish writer");

    // A byte budget far smaller than the entry: it must still come back
    // whole, since a budget only limits how many *additional* entries are
    // returned, never truncates the first one.
    let snapshot = conversations
        .read("attempt-1", 0, 1024)
        .expect("read")
        .expect("record must exist");
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].raw.len(), big.len());
    assert_eq!(snapshot.entries[0].raw, big);
}

// --- Error path --------------------------------------------------------------

#[test]
fn neither_file_existing_returns_no_record_rather_than_an_error() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-8");

    let result = conversations
        .read("no-such-attempt", 0, 1 << 20)
        .expect("read must not error");
    assert!(result.is_none());
}

// --- Integration ---------------------------------------------------------

#[test]
fn startup_leaves_a_running_executions_open_attempt_untouched_and_compresses_a_closed_one() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-9", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");

    let log = store.event_log("exec-9");
    log.append_next(&execution.id, action_started("attempt-open"))
        .expect("append ActionStarted (open)");

    log.append_next(&execution.id, action_started("attempt-closed"))
        .expect("append ActionStarted (closed)");
    log.append_next(&execution.id, action_completed("attempt-closed"))
        .expect("append ActionCompleted (closed)");

    let conversations = store.conversation_store("exec-9");

    let mut open_writer = conversations.writer("attempt-open").expect("open writer");
    open_writer
        .append_narsor("still running", &[])
        .expect("append");
    open_writer.finish().expect("finish writer");

    let mut closed_writer = conversations.writer("attempt-closed").expect("open writer");
    closed_writer.append_narsor("done", &[]).expect("append");
    closed_writer.finish().expect("finish writer");

    let compressed = compress_closed_conversations(&store).expect("compression sweep");
    assert_eq!(
        compressed,
        vec![("exec-9".to_string(), "attempt-closed".to_string())]
    );

    assert!(
        conversations.dir().join("attempt-open.jsonl").exists(),
        "an attempt open in a Running execution must be left untouched"
    );
    assert!(!conversations.dir().join("attempt-open.jsonl.zst").exists());

    assert!(
        conversations
            .dir()
            .join("attempt-closed.jsonl.zst")
            .exists(),
        "an attempt closed in the log must be compressed"
    );
    assert!(!conversations.dir().join("attempt-closed.jsonl").exists());
}

#[test]
fn a_failed_attempt_that_will_retry_is_still_closed_for_compression() {
    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-10", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");

    let log = store.event_log("exec-10");
    log.append_next(&execution.id, action_started("attempt-1"))
        .expect("append ActionStarted");
    log.append_next(&execution.id, action_attempt_failed("attempt-1"))
        .expect("append ActionAttemptFailed");

    let conversations = store.conversation_store("exec-10");
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("will retry", &[]).expect("append");
    writer.finish().expect("finish writer");

    compress_closed_conversations(&store).expect("compression sweep");

    assert!(conversations.dir().join("attempt-1.jsonl.zst").exists());
    assert!(!conversations.dir().join("attempt-1.jsonl").exists());
}

// --- When a record was last written, and its summaries (U6) ---------------

#[test]
fn an_open_record_is_dated_by_its_file_and_a_closed_one_by_its_last_entry() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-11");
    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("first", &[]).expect("append");
    writer.append_narsor("second", &[]).expect("append");
    writer.finish().expect("finish writer");

    let open = conversations
        .read("attempt-1", 0, usize::MAX)
        .expect("read")
        .expect("record must exist");
    let modified = fs::metadata(conversations.dir().join("attempt-1.jsonl"))
        .and_then(|metadata| metadata.modified())
        .expect("mtime");
    assert_eq!(open.recorded_at, Some(modified.into()));

    conversations.close_attempt("attempt-1").expect("close");
    // Even a read of the tail only dates the record by its very last entry.
    let closed = conversations
        .read("attempt-1", 2, usize::MAX)
        .expect("read")
        .expect("record must exist");
    let all = conversations
        .read("attempt-1", 0, usize::MAX)
        .expect("read")
        .expect("record must exist");
    assert!(closed.closed);
    assert_eq!(
        closed.recorded_at,
        Some(all.entries.last().unwrap().timestamp)
    );
}

#[test]
fn summaries_from_storage_follow_the_timeline_and_date_the_newest_record() {
    use narsor_engine::engine::conversation_summary::{read_summaries, ConversationCache};

    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-12", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");
    let log = store.event_log("exec-12");
    for event in [
        action_started("attempt-b"),
        action_completed("attempt-b"),
        action_started("attempt-a"),
    ] {
        log.append_next(&execution.id, event).expect("append");
    }
    let conversations = store.conversation_store("exec-12");
    let mut closed = conversations.writer("attempt-b").expect("open writer");
    closed.append_narsor("one", &[]).expect("append");
    closed.append_narsor("two", &[]).expect("append");
    closed.finish().expect("finish writer");
    conversations.close_attempt("attempt-b").expect("close");
    let mut open = conversations.writer("attempt-a").expect("open writer");
    open.append_narsor("still going", &[]).expect("append");
    open.finish().expect("finish writer");

    let mut cache = ConversationCache::default();
    let summaries = read_summaries(&store, "exec-12", &mut cache).expect("summaries");
    let ids: Vec<_> = summaries
        .attempts
        .iter()
        .map(|attempt| (attempt.attempt_id.as_str(), attempt.entries, attempt.closed))
        .collect();
    assert_eq!(
        ids,
        [("attempt-b", 2, true), ("attempt-a", 1, false)],
        "in timeline order, not by name"
    );
    assert_eq!(summaries.attempts[0].state_id.as_deref(), Some("plan"));
    let open_written = conversations
        .read("attempt-a", 0, usize::MAX)
        .expect("read")
        .expect("record")
        .recorded_at;
    assert_eq!(
        summaries.recorded_at,
        summaries.attempts[0].last_entry_at.max(open_written),
        "the newest of the records"
    );
    // A cached closed attempt is summarised the same the second time.
    assert_eq!(
        read_summaries(&store, "exec-12", &mut cache).expect("again"),
        summaries
    );
}

#[test]
fn an_execution_without_records_has_no_summaries_and_no_age() {
    use narsor_engine::engine::conversation_summary::{read_summaries, ConversationCache};

    let root = TempRoot::new();
    let store = root.store();
    let execution = sample_execution("exec-13", ExecutionStatus::Running);
    store.save_execution(&execution).expect("save execution");
    let summaries =
        read_summaries(&store, "exec-13", &mut ConversationCache::default()).expect("summaries");
    assert!(summaries.attempts.is_empty());
    assert_eq!(summaries.recorded_at, None);
}
