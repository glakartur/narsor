//! Client-safe projections shared with runner and controller protocol adapters.

use crate::persistence::{ConversationSnapshot, ConversationSource};
use narsor_protocol::api::{ConversationEntryDto, ConversationEntrySource, ConversationResponse};

/// Converts stored conversation bytes to the protocol response.
pub fn conversation_response_from_snapshot(
    snapshot: Option<ConversationSnapshot>,
) -> ConversationResponse {
    match snapshot {
        Some(snapshot) => ConversationResponse::Recorded {
            entries: snapshot
                .entries
                .into_iter()
                .map(|entry| ConversationEntryDto {
                    entry_number: entry.entry_number,
                    timestamp: entry.timestamp,
                    source: match entry.source {
                        ConversationSource::Narsor => ConversationEntrySource::Narsor,
                        ConversationSource::Stdout => ConversationEntrySource::Stdout,
                        ConversationSource::Stderr => ConversationEntrySource::Stderr,
                    },
                    raw_base64: base64_encode(&entry.raw),
                })
                .collect(),
            next_entry: snapshot.next_entry,
            closed: snapshot.closed,
            truncated_tail: snapshot.truncated_tail,
            source: None,
            recorded_at: None,
        },
        None => ConversationResponse::NoRecord,
    }
}

/// Storage root for the local controller's task directories.
pub fn local_task_storage_root(work_dir: &std::path::Path) -> std::path::PathBuf {
    work_dir.join("local-controller").join("storage")
}

fn base64_encode(raw: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    STANDARD.encode(raw)
}
