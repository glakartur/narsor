//! Reading an observer record of version 1 (watch lists) as version 2
//! (queues and run modes), KTD-8.
//!
//! ```text
//! version 1                          version 2
//! config.queue absent              → the observer's name
//! config.watch_mode watch_list     → config.mode on_episodes
//! config.watch_mode all or absent  → config.mode always
//! config.watch_objects             → dropped (the script checks its params)
//! config.effect deliver_event      → config.effect none; the observer is
//!                                    paused (a draft stays a draft) and its
//!                                    note says why; nothing is deleted
//! watch_round                      → dropped (old entries are not queue
//!                                    entries)
//! active_run of a watch run        → dropped: its token stops working and
//!                                    its result is refused
//! config.schedule.interval_secs    → unchanged
//! ```
//!
//! The store migrates in memory whenever it reads a version 1 record; the
//! next save writes version 2. Migrating twice gives the same record, so a
//! record that is read many times before its first save is read the same
//! way each time.

use serde_json::{Map, Value};

/// The note a version 1 observer with `effect: deliver_event` gets.
pub const DELIVER_EVENT_REMOVED_NOTE: &str =
    "paused by the update to observer queues: the deliver_event effect and `wait` steps were \
     removed. Change the script to answer the gate of its queue entry with \
     `narsor-cli task decide`, set the observer's queue to the queue of the gate, then resume it.";

/// Rewrites the version 1 record `record` (a JSON object) as version 2.
/// Anything that is not an object is left for deserialization to refuse.
pub fn migrate_v1_record(record: &mut Value) {
    let Some(record) = record.as_object_mut() else {
        return;
    };
    let mut deliver_event = false;
    if let Some(config) = record.get_mut("config").and_then(Value::as_object_mut) {
        deliver_event = migrate_config(config);
    }
    record.remove("watch_round");
    let watch_run = record
        .get("active_run")
        .and_then(Value::as_object)
        .is_some_and(|run| run.contains_key("watch"));
    if watch_run {
        record.insert("active_run".into(), Value::Null);
        record.insert("run_due_since".into(), Value::Null);
    }
    if deliver_event {
        let state = record
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("draft");
        if !matches!(state, "draft" | "paused") {
            record.insert("state".into(), Value::String("paused".into()));
        }
        record.insert("run_due_since".into(), Value::Null);
        record.insert(
            "note".into(),
            Value::String(DELIVER_EVENT_REMOVED_NOTE.into()),
        );
    }
    record.insert("record_version".into(), Value::from(2));
}

/// Migrates the configuration; `true` when its effect was `deliver_event`.
fn migrate_config(config: &mut Map<String, Value>) -> bool {
    if !config.contains_key("queue") {
        let name = config.get("name").cloned().unwrap_or(Value::Null);
        config.insert("queue".into(), name);
    }
    let watch_mode = config.remove("watch_mode");
    if !config.contains_key("mode") {
        let mode = match watch_mode.as_ref().and_then(Value::as_str) {
            Some("watch_list") => "on_episodes",
            _ => "always",
        };
        config.insert("mode".into(), Value::String(mode.into()));
    }
    config.remove("watch_objects");
    let deliver_event = config
        .get("effect")
        .and_then(|effect| effect.get("kind"))
        .and_then(Value::as_str)
        == Some("deliver_event");
    if deliver_event {
        config.insert("effect".into(), serde_json::json!({"kind": "none"}));
    }
    deliver_event
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn migrated(mut record: Value) -> Value {
        migrate_v1_record(&mut record);
        record
    }

    #[test]
    fn a_task_creating_observer_gets_its_name_as_queue_and_runs_always() {
        let record = migrated(json!({
            "record_version": 1,
            "state": "active",
            "config": {
                "name": "jira-new",
                "effect": {"kind": "create_task", "workflow_id": "triage", "workflow_version": 1},
                "watch_mode": "all",
                "watch_objects": {"allowed_prefixes": [], "max_len": 512},
                "schedule": {"interval_secs": 600}
            }
        }));
        assert_eq!(record["record_version"], 2);
        assert_eq!(record["state"], "active");
        assert_eq!(record["config"]["queue"], "jira-new");
        assert_eq!(record["config"]["mode"], "always");
        assert_eq!(record["config"]["schedule"], json!({"interval_secs": 600}));
        assert!(record["config"].get("watch_mode").is_none());
        assert!(record["config"].get("watch_objects").is_none());
        assert!(record.get("note").is_none());
    }

    #[test]
    fn a_deliver_event_observer_is_paused_with_a_note_not_deleted() {
        let record = migrated(json!({
            "record_version": 1,
            "state": "active",
            "run_due_since": "2026-11-13T12:00:00Z",
            "watch_round": ["abc"],
            "active_run": {"sequence": 3, "watch": {"entry_id": "abc", "object": "x"}},
            "config": {
                "name": "mr-review",
                "effect": {"kind": "deliver_event"},
                "watch_mode": "watch_list"
            }
        }));
        assert_eq!(record["state"], "paused");
        assert_eq!(record["note"], DELIVER_EVENT_REMOVED_NOTE);
        assert_eq!(record["config"]["effect"], json!({"kind": "none"}));
        assert_eq!(record["config"]["mode"], "on_episodes");
        assert_eq!(record["active_run"], Value::Null);
        assert_eq!(record["run_due_since"], Value::Null);
        assert!(record.get("watch_round").is_none());
    }

    #[test]
    fn a_deliver_event_draft_stays_a_draft_and_migrating_twice_changes_nothing() {
        let once = migrated(json!({
            "record_version": 1,
            "state": "draft",
            "config": {"name": "mr", "effect": {"kind": "deliver_event"}, "watch_mode": "watch_list"}
        }));
        assert_eq!(once["state"], "draft");
        assert_eq!(once["note"], DELIVER_EVENT_REMOVED_NOTE);
        assert_eq!(migrated(once.clone()), once);
    }
}
