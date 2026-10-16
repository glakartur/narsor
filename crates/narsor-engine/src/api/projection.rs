//! Client-safe projections shared with runner and controller protocol adapters.

use crate::persistence::{ConversationEntry, ConversationSnapshot, ConversationSource};
use narsor_protocol::api::{ConversationEntryDto, ConversationEntrySource, ConversationResponse};
use narsor_protocol::chat::{ChatElement, UnclassifiedReason};

/// Converts stored conversation bytes to the protocol response.
///
/// Every entry leaves with at least one element: one recorded before
/// elements existed gets a stand-in (see [`elements_of`]). Nothing is
/// migrated on disk.
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
                    elements: elements_of(&entry),
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

/// The elements an entry is shown with: the recorded ones, or — for an
/// entry recorded before elements existed — a single stand-in, so a
/// consumer always has something to show without reading the raw line.
///
/// A stderr line needs no classification to say what it is; any other
/// legacy entry is [`UnclassifiedReason::NotRecorded`].
fn elements_of(entry: &ConversationEntry) -> Vec<ChatElement> {
    if !entry.elements.is_empty() {
        return entry.elements.clone();
    }
    match entry.source {
        ConversationSource::Stderr => {
            vec![crate::engine::actions::agent::stderr_element(&entry.raw)]
        }
        ConversationSource::Narsor | ConversationSource::Stdout => {
            vec![ChatElement::Unclassified {
                reason: UnclassifiedReason::NotRecorded,
            }]
        }
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
