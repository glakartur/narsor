//! Append-only event log of a single execution.
//!
//! One file per event, named `{sequence:08}.json`. The file name carries the
//! ordering, so reading the log never depends on file metadata or on the order
//! the filesystem happens to return entries in.

use super::{PersistenceError, Result};
use crate::domain::{Event, EventData, ExecutionId};
use crate::persistence::atomic::{
    cleanup_orphaned_temp_files, ensure_dir, read_json, write_json_atomic_exclusive,
};
use crate::persistence::paths::{event_file_name, parse_event_file_name};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The first sequence number of every execution.
pub const FIRST_SEQUENCE: u64 = 1;

/// Append-only log of the events of one execution.
///
/// The next sequence number is derived from the directory listing **once**, on
/// first use, and then kept in memory. Listing on every append would make the
/// cost of appending grow linearly with the length of the execution, and a
/// counter stored in `state.json` would be a second source of truth that can
/// drift from the log it describes. The listing stays the authority; the
/// in-memory value is only a cache, and [`EventLog::append`] refuses to
/// overwrite an existing file, so a stale cache surfaces as an error instead of
/// silent data loss.
#[derive(Debug)]
pub struct EventLog {
    dir: PathBuf,
    next_sequence: Mutex<Option<u64>>,
}

impl EventLog {
    /// Opens the log stored in `dir`. The directory is created on first write.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            next_sequence: Mutex::new(None),
        }
    }

    /// The directory backing this log.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends `event` under its own sequence number.
    ///
    /// Fails with [`PersistenceError::EventAlreadyExists`] if that number is
    /// already taken; the existing file is then left untouched. That is the
    /// only defence against a drifted counter silently rewriting history.
    pub fn append(&self, event: &Event) -> Result<()> {
        let path = self.event_path(event.sequence);
        if !write_json_atomic_exclusive(&path, event)? {
            return Err(PersistenceError::EventAlreadyExists {
                sequence: event.sequence,
                path,
            });
        }
        self.observe_sequence(event.sequence);
        Ok(())
    }

    /// Builds and appends an event with the next free sequence number.
    ///
    /// Retries on a collision instead of failing outright: a second
    /// `EventLog` handle on the same directory — a second concurrent caller
    /// of [`crate::engine::Engine::run`]/`resume`/`deliver_human_input` now
    /// legitimately exists, guarded against advancing the same execution
    /// twice but not against *this* — starts its own next-sequence cache from
    /// an independent directory listing, so two handles can compute the same
    /// "next" number. The cache is only ever a hint the filesystem can
    /// rebuild (see [`EventLog`]'s own doc comment), so losing the race for
    /// one slot just means resyncing and claiming the next free one, rather
    /// than an event silently never reaching the log.
    pub fn append_next(&self, execution_id: &ExecutionId, data: EventData) -> Result<Event> {
        loop {
            let event = Event::new(execution_id.clone(), data.clone(), self.next_sequence()?);
            match self.append(&event) {
                Ok(()) => return Ok(event),
                Err(PersistenceError::EventAlreadyExists { .. }) => {
                    *self.lock_next_sequence() = None;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Reads the whole log, ordered by sequence number.
    ///
    /// An empty or missing directory yields an empty list. A hole in the
    /// numbering is corruption (`DOMAIN_MODEL.md` §5: monotonic, no gaps) and
    /// is reported as [`PersistenceError::SequenceGap`].
    pub fn read_all(&self) -> Result<Vec<Event>> {
        let sequences = self.sequence_numbers()?;
        let mut events = Vec::with_capacity(sequences.len());

        for (index, sequence) in sequences.iter().copied().enumerate() {
            let expected = FIRST_SEQUENCE + index as u64;
            if sequence != expected {
                return Err(PersistenceError::SequenceGap {
                    path: self.dir.clone(),
                    expected,
                    found: sequence,
                });
            }

            let path = self.event_path(sequence);
            let event: Event = read_json(&path)?;
            if event.sequence != sequence {
                return Err(PersistenceError::SequenceMismatch {
                    path,
                    expected: sequence,
                    declared: event.sequence,
                });
            }
            events.push(event);
        }

        Ok(events)
    }

    /// Returns the sequence numbers present on disk, ascending.
    ///
    /// Entries that are not well-formed event files (notably leftover `.tmp`
    /// files) are ignored.
    pub fn sequence_numbers(&self) -> Result<Vec<u64>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(PersistenceError::io(&self.dir, source)),
        };

        let mut sequences = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PersistenceError::io(&self.dir, source))?;
            let name = entry.file_name();
            if let Some(sequence) = name.to_str().and_then(parse_event_file_name) {
                sequences.push(sequence);
            }
        }
        sequences.sort_unstable();
        Ok(sequences)
    }

    /// The highest sequence number on disk, if the log is not empty.
    pub fn last_sequence(&self) -> Result<Option<u64>> {
        Ok(self.sequence_numbers()?.last().copied())
    }

    /// The sequence number the next appended event should carry.
    pub fn next_sequence(&self) -> Result<u64> {
        let mut cached = self.lock_next_sequence();
        if let Some(next) = *cached {
            return Ok(next);
        }
        let next = self
            .last_sequence()?
            .map_or(FIRST_SEQUENCE, |last| last.saturating_add(1));
        *cached = Some(next);
        Ok(next)
    }

    /// Removes `.tmp` files left behind by an interrupted event write.
    pub fn cleanup_orphaned_temp_files(&self) -> Result<Vec<PathBuf>> {
        cleanup_orphaned_temp_files(&self.dir)
    }

    /// Creates the log directory, so that readers see it even before the first
    /// event is appended.
    pub fn ensure_dir(&self) -> Result<()> {
        ensure_dir(&self.dir)
    }

    /// Path of the file holding the event with `sequence`.
    pub fn event_path(&self, sequence: u64) -> PathBuf {
        self.dir.join(event_file_name(sequence))
    }

    /// Advances the cached counter past a sequence number just written.
    fn observe_sequence(&self, sequence: u64) {
        let mut cached = self.lock_next_sequence();
        let next = sequence.saturating_add(1);
        *cached = Some(match *cached {
            Some(current) if current > next => current,
            _ => next,
        });
    }

    /// Locks the cached counter, recovering from a poisoned mutex.
    ///
    /// The cache is a hint that the filesystem can always rebuild, so a panic
    /// in another thread is no reason to make the log unusable.
    fn lock_next_sequence(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.next_sequence
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
