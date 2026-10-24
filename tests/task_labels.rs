//! Ticket and title of a task: normalization and the sealed launch spec.

use narsor_engine::domain::{
    effective_title, normalize_label, one_line_label, LabelError, ResolvedEnvironment,
    ResourceProfile, TaskLaunchSpec, TaskWorkspace, TICKET_MAX_CHARS, TITLE_MAX_CHARS,
};

fn spec() -> TaskLaunchSpec {
    TaskLaunchSpec::new(
        "wf",
        1,
        serde_json::json!({}),
        TaskWorkspace::None,
        ResolvedEnvironment::harness_only("claude-code"),
        ResourceProfile::new(1000, 2048),
    )
}

#[test]
fn labels_are_trimmed_and_empty_means_none() {
    let norm = |raw| normalize_label("ticket", raw, TICKET_MAX_CHARS);
    assert_eq!(norm(Some("  NAR-12  ")), Ok(Some("NAR-12".into())));
    assert_eq!(norm(Some("")), Ok(None));
    assert_eq!(norm(Some("   ")), Ok(None));
    assert_eq!(norm(None), Ok(None));
}

#[test]
fn a_label_with_a_line_break_is_refused() {
    assert_eq!(
        normalize_label("ticket", Some("NAR-12\nx"), TICKET_MAX_CHARS),
        Err(LabelError::NotOneLine { field: "ticket" })
    );
}

#[test]
fn limits_count_characters_not_bytes() {
    let ticket = |n| "ż".repeat(n);
    assert!(normalize_label("ticket", Some(&ticket(64)), TICKET_MAX_CHARS).is_ok());
    assert!(matches!(
        normalize_label("ticket", Some(&ticket(65)), TICKET_MAX_CHARS),
        Err(LabelError::TooLong { max: 64, .. })
    ));
    assert!(normalize_label("title", Some(&"a".repeat(200)), TITLE_MAX_CHARS).is_ok());
    assert!(normalize_label("title", Some(&"a".repeat(201)), TITLE_MAX_CHARS).is_err());
}

#[test]
fn a_spec_with_labels_has_a_valid_digest_and_a_new_one() {
    let plain = spec();
    let labelled = spec().with_labels(Some("NAR-12".into()), Some("Zmiana API".into()));
    assert!(labelled.digest_is_valid());
    assert_ne!(plain.digest, labelled.digest);
    assert_eq!(labelled.ticket.as_deref(), Some("NAR-12"));
}

#[test]
fn a_spec_sealed_without_labels_reads_back_and_keeps_its_digest() {
    let plain = spec();
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("ticket").is_none() && json.get("title").is_none());
    let back: TaskLaunchSpec = serde_json::from_value(json).unwrap();
    assert!(back.digest_is_valid());
    assert_eq!(back.ticket, None);
    assert_eq!(back.title, None);
    assert_eq!(back.digest, plain.digest);
}

#[test]
fn the_operator_title_wins_over_the_generated_one() {
    assert_eq!(effective_title(Some("A"), Some("B")).as_deref(), Some("A"));
    assert_eq!(effective_title(None, Some("B")).as_deref(), Some("B"));
    assert_eq!(effective_title(None, None), None);
}

#[test]
fn text_from_outside_becomes_one_short_line() {
    let text = format!("Zmiana\n\u{1b}[31m  API  {}", "x".repeat(80));
    let line = one_line_label(&text, 50).unwrap();
    assert!(line.chars().count() <= 50);
    assert!(!line.contains('\n') && !line.contains('\u{1b}'));
    assert_eq!(one_line_label(" \n ", 50), None);
}

fn full_spec() -> TaskLaunchSpec {
    let mut spec = spec().with_labels(Some("NAR-12".into()), Some("Zmiana API".into()));
    spec.definition = Some(serde_json::json!({"id": "wf"}));
    spec.digest = spec.compute_digest();
    spec
}

#[test]
fn a_spec_with_labels_and_definition_round_trips() {
    let full = full_spec();
    let back: TaskLaunchSpec =
        serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
    assert_eq!(back, full);
    assert!(back.digest_is_valid());
}

#[test]
fn another_version_is_refused_with_both_versions() {
    let mut json = serde_json::to_value(spec()).unwrap();
    json["version"] = serde_json::json!(2);
    let error = serde_json::from_value::<TaskLaunchSpec>(json).unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("version 2") && text.contains("expected 3"),
        "{text}"
    );
}

#[test]
fn an_unknown_top_level_field_is_refused() {
    let mut json = serde_json::to_value(spec()).unwrap();
    json["surprise"] = serde_json::json!(1);
    let error = serde_json::from_value::<TaskLaunchSpec>(json).unwrap_err();
    assert!(error.to_string().contains("surprise"), "{error}");
}

#[test]
fn a_bad_workspace_error_names_the_field() {
    let mut json = serde_json::to_value(spec()).unwrap();
    json["workspace"] = serde_json::json!(42);
    let error = serde_json::from_value::<TaskLaunchSpec>(json).unwrap_err();
    assert!(error.to_string().contains("workspace"), "{error}");
}
