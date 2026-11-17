//! The wire forms of watch entries that are still on the protocol: the
//! `watch_refused` task event and the `watch` of a runner's `EventWait`.
//!
//! The `wait` step that asked for watch entries was removed from the
//! workflow schema (R5); its end-to-end tests went with it. The engine's
//! `wait` action and the runner's event protocol go next, with these types.

use narsor_engine::orchestration::TaskEventKind;
use serde_json::{json, Value};

const MR: &str = "https://git.example.com/a/-/merge_requests/17";
const OBSERVER: &str = "mr-review";

#[test]
fn a_refused_watch_reaches_an_old_client_as_unrecognized() {
    let event = json!({
        "type": "watch_refused",
        "step": "await_review",
        "observer": OBSERVER,
        "object": MR,
        "reason": "no observer named 'mr-review' belongs to the task's owner",
    });
    let kind: TaskEventKind = serde_json::from_value(event.clone()).unwrap();
    assert!(matches!(kind, TaskEventKind::WatchRefused { .. }));
    assert_eq!(serde_json::to_value(&kind).unwrap(), event);
    let unknown: TaskEventKind =
        serde_json::from_value(json!({"type": "something_newer", "x": 1})).unwrap();
    assert_eq!(unknown, TaskEventKind::Unrecognized);
}

#[test]
fn the_runner_reports_the_watch_with_its_wait() {
    let wait = json!({
        "episode_id": "e-1",
        "event_type": "review.approved",
        "filter": {"mr_url": MR},
        "since": "2026-11-13T10:00:00Z",
        "watch": {
            "step": "await_review",
            "observer": OBSERVER,
            "object": MR,
            "event_types": ["review.approved"],
        },
    });
    let parsed: narsor_protocol::controller::EventWait =
        serde_json::from_value(wait.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), wait);
    // A wait without a watch says nothing of one.
    let mut without: Value = wait;
    without.as_object_mut().unwrap().remove("watch");
    let parsed: narsor_protocol::controller::EventWait =
        serde_json::from_value(without.clone()).unwrap();
    assert_eq!(parsed.watch, None);
    assert_eq!(serde_json::to_value(&parsed).unwrap(), without);
}
