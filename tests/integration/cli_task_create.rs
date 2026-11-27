//! The request `narsor task create` builds from its arguments and
//! `--stdin`, and what it asks for in an observer's script
//! (`NARSOR_OBSERVER_RUN`, passed in rather than set).

use clap::Parser;
use narsor::cli::observer_effect::{effect_call, EffectCall};
use narsor::cli::remote::{create_request, TaskCommand};
use narsor::cli::{Cli, Command};
use narsor_protocol::controller::CreateTaskRequest;
use serde_json::json;

fn parse(arguments: &[&str]) -> Result<Cli, clap::Error> {
    let mut argv = vec!["narsor-cli", "task", "create"];
    argv.extend_from_slice(arguments);
    Cli::try_parse_from(argv)
}

fn request(arguments: &[&str]) -> CreateTaskRequest {
    request_with_stdin(arguments, None)
}

fn request_with_stdin(arguments: &[&str], stdin: Option<&str>) -> CreateTaskRequest {
    let Command::Task { command } = parse(arguments).expect("arguments parse").command else {
        panic!("not a task command");
    };
    create_request(&command, stdin)
        .expect("request builds")
        .expect("a catalogue workflow")
        .0
}

#[test]
fn repository_branch_and_commit_name_the_workspace_they_are_for() {
    let request = request(&[
        "flow",
        "--repository",
        "code=acme",
        "--branch",
        "code=develop",
        "--commit",
        "docs=0123abcd",
    ]);

    let code = &request.launch.workspaces["code"];
    assert_eq!(code.repository.as_deref(), Some("acme"));
    assert_eq!(code.branch.as_deref(), Some("develop"));
    assert_eq!(code.commit, None);
    assert_eq!(
        request.launch.workspaces["docs"].commit.as_deref(),
        Some("0123abcd")
    );
    assert_eq!(request.launch.workspaces.len(), 2);
}

#[test]
fn a_branch_that_reads_as_json_stays_text() {
    let request = request(&["flow", "--branch", "code=123", "--branch", "docs=true"]);

    assert_eq!(
        request.launch.workspaces["code"].branch.as_deref(),
        Some("123")
    );
    assert_eq!(
        request.launch.workspaces["docs"].branch.as_deref(),
        Some("true")
    );
}

#[test]
fn without_a_choice_the_request_has_no_workspaces_key() {
    let request = request(&["flow", "--input", "code=acme"]);

    let json = serde_json::to_value(&request.launch).expect("serializes");
    assert!(json.get("workspaces").is_none(), "{json}");
}

#[test]
fn a_branch_without_a_workspace_name_is_a_usage_error_with_the_format() {
    let Command::Task { command } = parse(&["flow", "--branch", "develop"])
        .expect("clap accepts the value")
        .command
    else {
        panic!("not a task command");
    };

    let error = create_request(&command, None).expect_err("a bare branch is refused");

    assert!(error.message.contains("--branch"), "{}", error.message);
    assert!(error.message.contains("name=value"), "{}", error.message);
}

#[test]
fn the_options_override_the_stdin_object_value_by_value() {
    let stdin = r#"{
        "workflow": "flow",
        "inputs": {"description": "from stdin", "kept": 1},
        "workspaces": {"code": {"repository": "acme", "branch": "main"}},
        "ticket": "ABC-1",
        "title": "from stdin"
    }"#;

    let request = request_with_stdin(
        &[
            "--stdin",
            "--input",
            "description=from the option",
            "--branch",
            "code=develop",
            "--title",
            "from the option",
        ],
        Some(stdin),
    );

    let launch = &request.launch;
    assert_eq!(launch.workflow_id, "flow");
    assert_eq!(
        launch.inputs,
        json!({"description": "from the option", "kept": 1})
    );
    assert_eq!(
        launch.workspaces["code"].repository.as_deref(),
        Some("acme")
    );
    assert_eq!(launch.workspaces["code"].branch.as_deref(), Some("develop"));
    assert_eq!(launch.ticket.as_deref(), Some("ABC-1"));
    assert_eq!(launch.title.as_deref(), Some("from the option"));
}

#[test]
fn a_stdin_object_without_a_workflow_or_with_an_unknown_field_is_a_usage_error() {
    for (stdin, expected) in [
        (r#"{"inputs": {}}"#, "WORKFLOW"),
        (r#"{"workflow": "flow", "data": {}}"#, "Invalid --stdin"),
    ] {
        let Command::Task { command } = parse(&["--stdin"]).unwrap().command else {
            panic!("not a task command");
        };
        let error = create_request(&command, Some(stdin)).expect_err("refused");
        assert_eq!(error.exit_code, 2, "{}", error.message);
        assert!(error.message.contains(expected), "{}", error.message);
    }
}

#[test]
fn a_definition_file_takes_workspace_choices() {
    let Command::Task { .. } = parse(&[
        "--file",
        "flow.yaml",
        "--repository",
        "code=acme",
        "--branch",
        "code=develop",
    ])
    .expect("a file takes workspace choices")
    .command
    else {
        panic!("not a task command");
    };
}

// ---------------------------------------------------------------------------
// The effect commands of an observer's script (U7)
// ---------------------------------------------------------------------------

/// The run id `NARSOR_OBSERVER_RUN` holds in a script.
const RUN: &str = "obs-0123456789abcdef0123456789abcdef.3";

fn task_command(arguments: &[&str]) -> TaskCommand {
    let mut argv = vec!["narsor-cli", "task"];
    argv.extend_from_slice(arguments);
    let Command::Task { command } = Cli::try_parse_from(argv).expect("arguments parse").command
    else {
        panic!("not a task command");
    };
    command
}

#[test]
fn outside_an_observer_run_task_create_goes_its_usual_way() {
    let command = task_command(&["create", "flow", "--input", "code=acme"]);

    assert_eq!(effect_call(&command, None, None).unwrap(), None);
    assert_eq!(effect_call(&command, Some(""), None).unwrap(), None);
}

#[test]
fn in_an_observer_run_task_create_sends_the_request_the_console_builds() {
    let arguments = [
        "create",
        "flow",
        "--input",
        "description=x",
        "--repository",
        "code=acme",
        "--ticket",
        "ABC-1",
    ];
    let console = create_request(&task_command(&arguments), None)
        .unwrap()
        .unwrap()
        .0;

    let call = effect_call(&task_command(&arguments), Some(RUN), None).unwrap();

    assert_eq!(call, Some(EffectCall::CreateTask(console)));
}

#[test]
fn in_an_observer_run_a_definition_file_is_a_usage_error() {
    let error = effect_call(
        &task_command(&["create", "--file", "flow.yaml"]),
        Some(RUN),
        None,
    )
    .expect_err("a local definition file");

    assert_eq!(error.exit_code, 2);
    assert!(error.message.contains("--file"), "{}", error.message);
}

#[test]
fn task_event_is_gone_with_the_wait_step() {
    let argv = [
        "narsor-cli",
        "task",
        "event",
        "task-1",
        "--type",
        "approved",
    ];
    assert!(Cli::try_parse_from(argv).is_err());
}
