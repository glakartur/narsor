//! The cutover from stored observers to catalogue automations (R22).
//!
//! Observers lived in `{old state}/observers/{id}/observer.json` with their
//! script versions and approvals. Automations and queues are catalogue
//! files now, and nothing migrates the old records: the catalogue author
//! writes them again as files. The layout migration (D82) leaves the old
//! directory at the old state root. The server never reads the old records;
//! at start it only says, once, that some are left
//! ([`stored_observers_warning`]), so an operator who updated without
//! writing the catalogue files knows why nothing runs.

use std::path::Path;

use crate::persistence::paths::{OBSERVERS_DIR, OBSERVER_RECORD_FILE};

/// The warning the server logs at start when `{old state root}/observers/`
/// still holds observer records, `None` when it holds none. Only the
/// directory listing is read: a record's content is never opened.
pub fn stored_observers_warning(old_state_root: &Path) -> Option<String> {
    let dir = old_state_root.join(OBSERVERS_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    let stored = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join(OBSERVER_RECORD_FILE).is_file())
        .count();
    (stored > 0).then(|| {
        format!(
            "{} holds {stored} observer record(s) of an older version; they are ignored. \
             Automations and queues are catalogue files now: write them under the \
             catalogue's automations/ and queues/ directories, then delete {}",
            dir.display(),
            dir.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_directory_with_observer_records_gets_a_warning() {
        let root = std::env::temp_dir().join(format!("narsor-migrate-{}", uuid::Uuid::new_v4()));
        assert_eq!(stored_observers_warning(&root), None);
        let observer = root.join(OBSERVERS_DIR).join("obs-1");
        std::fs::create_dir_all(&observer).unwrap();
        // A crash left a directory without a record: nothing to report.
        assert_eq!(stored_observers_warning(&root), None);
        std::fs::write(observer.join(OBSERVER_RECORD_FILE), "{ not even json").unwrap();
        let warning = stored_observers_warning(&root).unwrap();
        assert!(warning.contains("1 observer record(s)"), "{warning}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
