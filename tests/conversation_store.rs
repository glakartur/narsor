//! Integration tests for the per-attempt conversation store (U2).
//!
//! Every test runs against a real temporary directory: the whole value of
//! this layer lies in real file operations (append, compress, rename), so
//! nothing here is mocked.

use chrono::Utc;
use narsor::domain::{
    Context, EventData, ExecutionStatus, RepositoryInfo, WorkflowExecution, Workspace,
};
use narsor::persistence::conversation::ConversationSource;
use narsor::persistence::recovery::compress_closed_conversations;
use narsor::persistence::ExecutionStore;
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
        .append_narsor("prompt: do the thing")
        .expect("append prompt");
    writer
        .append(ConversationSource::Stdout, br#"{"type":"assistant"}"#)
        .expect("append stdout");
    writer
        .append_narsor("exit code: 0")
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
fn reading_an_open_attempt_from_a_cursor_returns_only_new_entries() {
    let root = TempRoot::new();
    let store = root.store();
    let conversations = store.conversation_store("exec-2");

    let mut writer = conversations.writer("attempt-1").expect("open writer");
    writer.append_narsor("one").expect("append 1");
    writer.append_narsor("two").expect("append 2");

    let first_read = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(first_read.entries.len(), 2);
    assert!(!first_read.closed);
    let cursor = first_read.next_entry;
    assert_eq!(cursor, 3);

    writer.append_narsor("three").expect("append 3");
    writer.append_narsor("four").expect("append 4");

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
    writer.append_narsor("complete entry").expect("append");
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
    writer.append_narsor("one").expect("append 1");
    writer.append_narsor("two").expect("append 2");

    let first_read = conversations
        .read("attempt-1", 0, 1 << 20)
        .expect("read")
        .expect("record must exist");
    assert_eq!(first_read.entries.len(), 2);
    let cursor = first_read.next_entry;

    writer.append_narsor("three").expect("append 3");
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
    writer.append_narsor("hello").expect("append");
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
        .append(ConversationSource::Stdout, not_json)
        .expect("append non-json line");
    writer
        .append(ConversationSource::Stdout, invalid_utf8)
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
        .append(ConversationSource::Stdout, &big)
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
    open_writer.append_narsor("still running").expect("append");
    open_writer.finish().expect("finish writer");

    let mut closed_writer = conversations.writer("attempt-closed").expect("open writer");
    closed_writer.append_narsor("done").expect("append");
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
    writer.append_narsor("will retry").expect("append");
    writer.finish().expect("finish writer");

    compress_closed_conversations(&store).expect("compression sweep");

    assert!(conversations.dir().join("attempt-1.jsonl.zst").exists());
    assert!(!conversations.dir().join("attempt-1.jsonl").exists());
}
