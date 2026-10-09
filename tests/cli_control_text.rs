//! The text `narsor task history|workflows|validate|capabilities|repo` print
//! (`narsor::cli::control_text`) and the typing of `task create --file` inputs.

use narsor::cli::control_text::{
    capabilities_lines, history_detail_lines, history_row, repository_row, typed_inputs,
    validation_lines, workflow_lines,
};
use narsor::controller_protocol::{ValidateWorkflowResponse, WorkflowInputView, WorkflowSource};
use narsor::domain::ParamType;
use serde_json::json;

mod task_app_support;
use task_app_support::{capabilities, catalogue_workflow, legacy, repository, workflow_input};

fn input(name: &str, kind: ParamType, required: bool) -> WorkflowInputView {
    workflow_input(name, kind, required, None)
}

fn texts(lines: Vec<narsor::cli::task_text::ToneLine>) -> Vec<String> {
    lines.into_iter().map(|line| line.text).collect()
}

#[test]
fn inputs_are_typed_by_what_the_definition_declares() {
    let mut greeting = input("greeting", ParamType::String, true);
    greeting.default = None;
    let mut flag = input("flag", ParamType::Boolean, false);
    flag.default = Some(json!(false));
    let declared = vec![
        greeting,
        input("count", ParamType::Integer, false),
        flag,
        input("repo", ParamType::Repository, false),
    ];
    let values = typed_inputs(
        &declared,
        &["greeting=42".into(), "count=7".into(), "repo=api".into()],
    )
    .unwrap();
    assert_eq!(values["greeting"], json!("42"), "a string stays text");
    assert_eq!(values["count"], json!(7));
    assert_eq!(values["flag"], json!(false), "the default fills what is left out");
    assert_eq!(values["repo"], json!("api"));
}

#[test]
fn a_wrongly_typed_unknown_or_missing_input_is_a_usage_error() {
    let declared = vec![
        input("greeting", ParamType::String, true),
        input("count", ParamType::Integer, true),
        input("flag", ParamType::Boolean, false),
    ];
    let wrong = typed_inputs(&declared, &["count=many".into()]).unwrap_err();
    assert!(wrong.message.contains("not a whole number"), "{wrong:?}");
    let flag = typed_inputs(&declared, &["flag=maybe".into()]).unwrap_err();
    assert!(flag.message.contains("boolean"), "{flag:?}");
    let unknown = typed_inputs(&declared, &["nope=1".into()]).unwrap_err();
    assert!(unknown.message.contains("greeting, count, flag"), "{unknown:?}");
    let missing = typed_inputs(&declared, &[]).unwrap_err();
    assert!(
        missing.message.contains("greeting, count"),
        "every missing input at once: {missing:?}"
    );
    let malformed = typed_inputs(&declared, &["greeting".into()]).unwrap_err();
    assert!(malformed.message.contains("name=value"), "{malformed:?}");
}

#[test]
fn a_workflow_of_the_catalogue_shows_source_description_and_inputs() {
    let mut workflow = catalogue_workflow(
        "implement-feature",
        "Builds a feature.",
        vec![input("ticket", ParamType::String, true)],
    );
    workflow.overrides = true;
    let text = texts(workflow_lines(&workflow));
    assert_eq!(
        text[0],
        "implement-feature v1  [built-in]  (hides another copy of the same name)"
    );
    assert_eq!(text[1], "    Builds a feature.");
    assert_eq!(text[2], "    input ticket: String required");

    let mut broken = catalogue_workflow("broken", "x", Vec::new());
    broken.valid = false;
    broken.source = WorkflowSource::User;
    broken.violations = vec!["state 'a' leads nowhere".into()];
    let text = texts(workflow_lines(&broken));
    assert!(text[0].ends_with("[user]  INVALID"), "{text:?}");
    assert!(text.iter().any(|line| line.contains("leads nowhere")));
}

#[test]
fn a_verdict_lists_every_violation_or_the_definition() {
    let invalid = ValidateWorkflowResponse {
        valid: false,
        name: None,
        version: None,
        inputs: Vec::new(),
        violations: vec!["first".into(), "second".into()],
    };
    assert_eq!(
        texts(validation_lines(&invalid)),
        ["Invalid — 2 violation(s):", "  - first", "  - second"]
    );
    let valid = ValidateWorkflowResponse {
        valid: true,
        name: Some("greet".into()),
        version: Some(2),
        inputs: vec![input("who", ParamType::String, true)],
        violations: Vec::new(),
    };
    let text = texts(validation_lines(&valid));
    assert_eq!(text[0], "Valid: greet v2");
    assert_eq!(text[1], "    input who: String required");
}

#[test]
fn capabilities_say_what_is_refused_and_why() {
    let text = texts(capabilities_lines(&capabilities(false, Some("not a local daemon"))));
    assert_eq!(text[0], "Connection:             staging");
    assert_eq!(text[1], "Workflow paths:         no (not a local daemon)");
    assert_eq!(text[2], "Old executions:         no");
    let local = texts(capabilities_lines(&capabilities(true, None)));
    assert_eq!(local[1], "Workflow paths:         yes");
    assert_eq!(local[3], "Local repository paths: yes");
}

#[test]
fn an_old_execution_says_whether_it_can_be_recovered() {
    let failed = legacy("exec-1", "implement-feature", "failed", 0, true, None);
    let text = texts(history_detail_lines(&failed));
    assert!(text.iter().any(|line| line == "Execution  exec-1"), "{text:?}");
    assert!(text.iter().any(|line| line.contains("possible (narsor task history recover")));
    let done = legacy(
        "exec-2",
        "implement-feature",
        "completed",
        1,
        false,
        Some("a completed execution cannot be recovered"),
    );
    let text = texts(history_detail_lines(&done));
    assert!(
        text.iter().any(|line| line.contains("not possible: a completed execution")),
        "{text:?}"
    );
    let row = history_row(&done);
    assert!(row.starts_with("exec-2") && row.contains("completed") && row.ends_with("implement-feature v1"));
}

#[test]
fn a_repository_row_matches_the_local_listing() {
    let row = repository_row(&repository("api", "https://example.com/api.git"));
    assert!(row.starts_with("api "), "{row}");
    assert!(row.contains("github") && row.contains("main"), "{row}");
    assert!(row.ends_with("https://example.com/api.git"), "{row}");
}

#[test]
fn what_a_server_sends_never_reaches_a_terminal_as_control_sequences() {
    let hostile = "x\u{1b}]0;pwned\u{7}y\u{202e}z";
    let clean = |text: &str| !text.contains('\u{1b}') && !text.contains('\u{202e}');
    let mut workflow = catalogue_workflow(hostile, hostile, vec![input(hostile, ParamType::String, true)]);
    workflow.violations = vec![hostile.into()];
    for line in workflow_lines(&workflow) {
        assert!(clean(&line.text), "{:?}", line.text);
    }
    assert!(clean(&repository_row(&repository(hostile, hostile))));
    assert!(clean(&history_row(&legacy(hostile, hostile, "failed", 0, false, None))));
    for line in history_detail_lines(&legacy(hostile, hostile, "failed", 0, false, Some(hostile))) {
        assert!(clean(&line.text), "{:?}", line.text);
    }
}
