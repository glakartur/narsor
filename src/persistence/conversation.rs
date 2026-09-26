//! Per-attempt conversation storage: incremental JSONL, compressed on close.
//!
//! Every action attempt gets exactly one conversation file, keyed by its
//! `action_execution_id` (D24), under
//! `{state}/executions/{id}/conversations/`. This is a second, narrower store
//! next to the event log rather than a field on an event: an `AgentMessage`
//! event already gets truncated (the log is not a place for megabytes of
//! harness chatter), and R9 needs the *whole* stream preserved somewhere. The
//! event log stays the authority for what happened and when; this store is
//! only the authority for what the harness actually said, byte for byte.
//!
//! # Why not one JSON object per line, verbatim
//!
//! A harness line is not guaranteed to be valid UTF-8, and R9 requires it
//! survive "byte for byte" regardless. A JSON string can only hold valid
//! Unicode scalar values, so the raw line cannot be spliced into the envelope
//! unescaped. Instead every entry base64-encodes its payload: the envelope
//! itself stays trivially parseable JSON (`.jsonl` keeps its meaning), and the
//! payload round-trips exactly no matter what the harness printed.
//!
//! # Why plain-then-compress rather than one format
//!
//! An attempt is bounded in time and written by exactly one process. Writing
//! independent zstd frames incrementally would mean every reader has to
//! tolerate a partial trailing frame; writing plain JSONL while open and
//! compressing the whole thing once, atomically, at close means a reader only
//! ever has to tolerate a partial trailing *line* — a strictly smaller
//! problem, and one every text log already has. See
//! `docs/plans/2026-09-26-002-*.md`, Key Technical Decisions, for the fuller
//! argument (R15 "if practical").
//!
//! # Cursor semantics
//!
//! The cursor is an entry number, not a byte offset, precisely so a reader
//! polling across the plain-to-compressed transition does not have to know it
//! happened: entry numbers are stable, byte offsets inside a freshly
//! compressed file are not. A truncated last line — the trace of a writer
//! killed mid-`write_all`, never of interleaving, since every entry is written
//! with a single `write_all` — is silently withheld from the returned entries
//! either way; whether it is reported as `truncated_tail` follows the bytes
//! actually on disk, open or closed alike, because compression preserves
//! whatever was there, truncation included.

use super::atomic::{ensure_dir, sync_parent_dir, write_and_sync};
use super::paths::{parse_conversation_jsonl_name, unique_temp_path_for, StatePaths};
use super::{PersistenceError, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Where one raw line of a conversation came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationSource {
    /// A line narsor itself wrote: the prompt sent, or the attempt's outcome.
    Narsor,
    /// A line the harness process wrote to its standard output.
    Stdout,
    /// A line the harness process wrote to its standard error.
    Stderr,
}

/// The on-disk shape of one line: a fixed envelope plus a base64 payload.
///
/// Private and deliberately minimal — anything a reader needs beyond this is
/// derived, not stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    /// Sequential entry number, starting at 1, unique within one attempt.
    n: u64,
    /// When narsor observed (or produced) this line.
    ts: DateTime<Utc>,
    /// `narsor` | `stdout` | `stderr`.
    source: ConversationSource,
    /// Base64 of the raw bytes — see the module doc for why.
    raw_b64: String,
}

/// One decoded entry of a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationEntry {
    /// Sequential entry number, starting at 1.
    pub entry_number: u64,
    /// When narsor observed (or produced) this line.
    pub timestamp: DateTime<Utc>,
    /// Which stream this line came from.
    pub source: ConversationSource,
    /// The raw harness line, exactly as received — preserved byte for byte,
    /// even when it is not valid JSON or not valid UTF-8.
    pub raw: Vec<u8>,
}

/// The result of reading a conversation from a given cursor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConversationSnapshot {
    /// Entries from the requested cursor onward, in order.
    pub entries: Vec<ConversationEntry>,
    /// The entry number to pass as the cursor on the next read, to continue
    /// exactly where this one left off.
    pub next_entry: u64,
    /// Whether this attempt's conversation is finished (the file has been
    /// compressed to `.jsonl.zst`). `false` for an attempt still being
    /// written by a live process, even once every entry so far has been
    /// read.
    pub closed: bool,
    /// Whether the on-disk file's last line was incomplete when read — the
    /// trace of a writer killed mid-`write_all`. That line is never included
    /// in `entries`; this is the only signal that it existed.
    pub truncated_tail: bool,
}

/// Append-only writer for one attempt's conversation file.
///
/// Opens `{attempt_id}.jsonl` in append mode and writes every entry with a
/// single `write_all`, so a concurrent reader never observes a half-written
/// line — only the very last line of the file can ever be incomplete, and
/// only because the process died mid-write. `fsync` is deliberately not
/// called per entry; it happens once, in [`ConversationWriter::finish`].
///
/// A writer always starts a fresh attempt at entry 1. Resuming a partially
/// written file across a narsor restart is out of scope: an attempt still
/// open when the process dies is reconciled as an interrupted action (D23)
/// and is never replayed, so nothing is ever meant to keep appending to it.
pub struct ConversationWriter {
    file: File,
    path: PathBuf,
    next_entry: u64,
}

impl ConversationWriter {
    /// Opens the writer for `path`, creating parent directories as needed.
    pub fn create(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| PersistenceError::io(&path, source))?;
        restrict_to_owner(&path);
        Ok(Self {
            file,
            path,
            next_entry: 1,
        })
    }

    /// The file this writer is appending to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry and returns the entry number it was written as.
    pub fn append(&mut self, source: ConversationSource, raw: &[u8]) -> Result<u64> {
        let entry_number = self.next_entry;
        let envelope = Envelope {
            n: entry_number,
            ts: Utc::now(),
            source,
            raw_b64: BASE64.encode(raw),
        };
        let mut bytes =
            serde_json::to_vec(&envelope).map_err(|source| PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            })?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|source| PersistenceError::io(&self.path, source))?;
        self.next_entry += 1;
        Ok(entry_number)
    }

    /// Convenience for [`ConversationSource::Narsor`] entries: the sent
    /// prompt, or the attempt's outcome.
    pub fn append_narsor(&mut self, text: &str) -> Result<u64> {
        self.append(ConversationSource::Narsor, text.as_bytes())
    }

    /// Flushes every entry written so far all the way to the device, and
    /// consumes the writer. Call this once, when the attempt ends.
    pub fn finish(self) -> Result<()> {
        self.file
            .sync_data()
            .map_err(|source| PersistenceError::io(&self.path, source))
    }
}

/// Reads and writes the conversation files of one execution.
///
/// A thin wrapper over [`StatePaths`], mirroring how [`super::EventLog`] is
/// scoped to one execution's event directory — every path is still built in
/// exactly one place ([`StatePaths`]).
#[derive(Debug, Clone)]
pub struct ConversationStore {
    paths: StatePaths,
    execution_id: String,
}

impl ConversationStore {
    /// Creates a store scoped to `execution_id` within `paths`.
    pub fn new(paths: StatePaths, execution_id: impl Into<String>) -> Self {
        Self {
            paths,
            execution_id: execution_id.into(),
        }
    }

    /// `{state}/executions/{id}/conversations`
    pub fn dir(&self) -> PathBuf {
        self.paths.conversations_dir(&self.execution_id)
    }

    fn jsonl_path(&self, attempt_id: &str) -> PathBuf {
        self.paths.conversation_file(&self.execution_id, attempt_id)
    }

    fn zst_path(&self, attempt_id: &str) -> PathBuf {
        self.paths
            .conversation_compressed_file(&self.execution_id, attempt_id)
    }

    /// Opens a fresh writer for `attempt_id`, creating the directory as
    /// needed.
    pub fn writer(&self, attempt_id: &str) -> Result<ConversationWriter> {
        ConversationWriter::create(self.jsonl_path(attempt_id))
    }

    /// Closes an attempt: compresses its plain file to `.jsonl.zst`
    /// atomically (temp + fsync + rename, mirroring [`super::atomic`]), then
    /// deletes the plain file.
    ///
    /// Idempotent: if the plain file is already gone — already compressed, or
    /// never written at all (a template error before the executor ever ran,
    /// see the plan's Open Questions) — this is a harmless no-op.
    pub fn close_attempt(&self, attempt_id: &str) -> Result<()> {
        let jsonl_path = self.jsonl_path(attempt_id);
        if !jsonl_path.exists() {
            return Ok(());
        }

        let zst_path = self.zst_path(attempt_id);
        let plain = fs::read(&jsonl_path).map_err(|source| PersistenceError::io(&jsonl_path, source))?;
        let compressed = zstd::stream::encode_all(plain.as_slice(), 0)
            .map_err(|source| PersistenceError::io(&zst_path, source))?;

        let temp = unique_temp_path_for(&zst_path);
        write_and_sync(&temp, &compressed)?;
        restrict_to_owner(&temp);
        fs::rename(&temp, &zst_path).map_err(|source| {
            let _ = fs::remove_file(&temp);
            PersistenceError::io(&zst_path, source)
        })?;
        sync_parent_dir(&zst_path);

        // Tolerate a concurrent racer (another process reconciling the same
        // execution) having already removed the plain file after its own
        // rename landed first: the rename above already succeeded either way
        // (a second rename onto the same target is a harmless no-op on
        // POSIX), so the only thing left to be racy about is this delete,
        // and "it's already gone" is exactly the outcome we wanted.
        if let Err(source) = fs::remove_file(&jsonl_path) {
            if source.kind() != io::ErrorKind::NotFound {
                return Err(PersistenceError::io(&jsonl_path, source));
            }
        }
        Ok(())
    }

    /// Reads entries of `attempt_id` from `from_entry` (inclusive) onward, up
    /// to `byte_budget` bytes of raw payload.
    ///
    /// The first returned entry is always whole even if it alone exceeds the
    /// budget — a budget is a soft cap on the response, never a reason to
    /// truncate a single entry (R14's "no buffer limit clips the final
    /// result" applies just as much to a read as to a write).
    ///
    /// Returns `Ok(None)` when neither file exists — a step whose
    /// conversation never started (or belongs to a log written before this
    /// store existed), not an error.
    pub fn read(
        &self,
        attempt_id: &str,
        from_entry: u64,
        byte_budget: usize,
    ) -> Result<Option<ConversationSnapshot>> {
        let zst_path = self.zst_path(attempt_id);
        let jsonl_path = self.jsonl_path(attempt_id);

        // `close_attempt` renames the plain file to `.zst` and only then
        // deletes it, so there is a narrow window in which neither `exists()`
        // check below observes a file: this check ran before the rename, and
        // the next one ran after the delete. Retrying once, rather than
        // reporting "no record", is what makes that window invisible to a
        // reader instead of surfacing as a spurious gap in the conversation.
        let (bytes, closed) = match Self::read_whichever_exists(&zst_path, &jsonl_path)? {
            Some(found) => found,
            None => match Self::read_whichever_exists(&zst_path, &jsonl_path)? {
                Some(found) => found,
                None => return Ok(None),
            },
        };

        let source_path = if closed { &zst_path } else { &jsonl_path };
        let (lines, trailing) = split_lines(&bytes);
        let truncated_tail = trailing.is_some();

        let mut entries = Vec::new();
        let mut budget_used = 0usize;
        let mut next_entry = from_entry;

        for line in lines {
            let envelope: Envelope = serde_json::from_slice(line).map_err(|source| {
                PersistenceError::Deserialize {
                    path: source_path.clone(),
                    source,
                }
            })?;
            if envelope.n < from_entry {
                continue;
            }
            if !entries.is_empty() && budget_used >= byte_budget {
                break;
            }

            let raw = BASE64.decode(envelope.raw_b64.as_bytes()).map_err(|error| {
                PersistenceError::io(
                    source_path,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("entry {} has invalid base64 payload: {error}", envelope.n),
                    ),
                )
            })?;

            budget_used += raw.len();
            next_entry = envelope.n + 1;
            entries.push(ConversationEntry {
                entry_number: envelope.n,
                timestamp: envelope.ts,
                source: envelope.source,
                raw,
            });
        }

        Ok(Some(ConversationSnapshot {
            entries,
            next_entry,
            closed,
            truncated_tail,
        }))
    }

    /// Reads whichever file currently exists (`.zst` preferred), returning
    /// the raw bytes and whether they came from the closed (compressed)
    /// form. `Ok(None)` means neither file was present at the moment of the
    /// check — the caller decides whether that is final or worth a retry.
    fn read_whichever_exists(
        zst_path: &Path,
        jsonl_path: &Path,
    ) -> Result<Option<(Vec<u8>, bool)>> {
        if zst_path.exists() {
            let file =
                File::open(zst_path).map_err(|source| PersistenceError::io(zst_path, source))?;
            let bytes = zstd::stream::decode_all(file)
                .map_err(|source| PersistenceError::io(zst_path, source))?;
            Ok(Some((bytes, true)))
        } else if jsonl_path.exists() {
            let bytes =
                fs::read(jsonl_path).map_err(|source| PersistenceError::io(jsonl_path, source))?;
            Ok(Some((bytes, false)))
        } else {
            Ok(None)
        }
    }

    /// Attempt ids with a plain (not yet compressed) conversation file.
    ///
    /// Used by [`super::recovery`] to find candidates for the startup
    /// compression sweep.
    pub fn open_attempt_ids(&self) -> Result<Vec<String>> {
        let dir = self.dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(PersistenceError::io(&dir, source)),
        };

        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PersistenceError::io(&dir, source))?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = parse_conversation_jsonl_name(name) {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Every attempt id with a conversation record on disk, open or closed —
    /// exactly the `conversation_presence` set [`crate::engine::build_timeline`]
    /// needs, and broader than [`Self::open_attempt_ids`], which deliberately
    /// only covers the startup compression sweep's narrower "still plain"
    /// question. Shared by the axum API's timeline handler and the `steps`
    /// CLI command so the two can never compute a different answer for the
    /// same execution.
    pub fn recorded_attempt_ids(&self) -> Result<std::collections::HashSet<String>> {
        let dir = self.dir();
        let mut ids = std::collections::HashSet::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(ids),
            Err(source) => return Err(PersistenceError::io(&dir, source)),
        };
        for entry in entries {
            let entry = entry.map_err(|source| PersistenceError::io(&dir, source))?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Some(id) = name.strip_suffix(super::paths::CONVERSATION_ZST_SUFFIX) {
                ids.insert(id.to_string());
            } else if let Some(id) = parse_conversation_jsonl_name(&name) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }
}

/// Restricts `path` to owner-only read/write (`0600`).
///
/// A conversation record is raw harness stdout/stderr — the same
/// sensitivity class as the workspace, per the plan, and can carry secrets
/// a tool call touched. `ConversationWriter::create` and
/// [`ConversationStore::close_attempt`] both call this on every file they
/// produce, mirroring the permission-tightening pattern already used for
/// read-only workspaces (`src/workspace/provisioner.rs`).
///
/// Best-effort only, like every other write in this module: a failure here
/// is logged, never propagated, so a permissions quirk on an unusual
/// filesystem cannot fail the run.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o600);
    if let Err(error) = fs::set_permissions(path, permissions) {
        tracing::warn!(path = %path.display(), error = %error, "conversation: could not restrict file permissions");
    }
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

/// Splits `bytes` on `\n` into complete lines and an optional trailing
/// incomplete chunk (present only when `bytes` does not end with `\n` and
/// what follows the last `\n` is non-empty).
fn split_lines(bytes: &[u8]) -> (Vec<&[u8]>, Option<&[u8]>) {
    if bytes.is_empty() {
        return (Vec::new(), None);
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');
    let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    // `split` yields a trailing empty slice whenever the input ends with the
    // separator; drop it so a well-formed file reports no trailing chunk.
    let trailing = lines.pop();
    if ends_with_newline {
        (lines, None)
    } else {
        (lines, trailing.filter(|chunk| !chunk.is_empty()))
    }
}
