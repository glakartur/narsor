//! The request `narsor task create` builds from its arguments, and what
//! `task create --data` asks for in an observer's script
//! (`NARSOR_OBSERVER_RUN`, passed in rather than set).

use clap::Parser;
use narsor::cli::observer_effect::{effect_call, EffectCall};
use narsor::cli::remote::{create_request, TaskCommand};
use narsor::cli::{Cli, Command};
use narsor_protocol::controller::{CreateTaskRequest, ObserverCreateTaskRequest};
use serde_json::json;

fn parse(arguments: &[&str]) -> Result<Cli, clap::Error> {
    let mut argv = vec!["narsor-cli", "task", "create"];
    argv.extend_from_slice(arguments);
    Cli::try_parse_from(argv)
}

fn request(arguments: &[&str]) -> CreateTaskRequest {
    let Command::Task { command } = parse(arguments).expect("arguments parse").command else {
        panic!("not a task command");
    };
    create_request(&command)
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

    let error = create_request(&command).expect_err("a bare branch is refused");

    assert!(error.message.contains("--branch"), "{}", error.message);
    assert!(error.message.contains("name=value"), "{}", error.message);
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

fn usage_error(arguments: &[&str], observer_run: Option<&str>) -> String {
    let error = effect_call(&task_command(arguments), observer_run).expect_err("a usage error");
    assert_eq!(error.exit_code, 2, "{}", error.message);
    error.message
}

#[test]
fn data_outside_an_observer_run_is_a_usage_error() {
    let message = usage_error(&["create", "--data", "{}"], None);
    assert!(message.contains("--data"), "{message}");
    assert!(message.contains("NARSOR_OBSERVER_RUN"), "{message}");

    // An empty variable is no run either.
    usage_error(&["create", "--data", "{}"], Some(""));
}

#[test]
fn outside_an_observer_run_task_create_goes_its_usual_way() {
    let command = task_command(&["create", "flow", "--input", "code=acme"]);

    assert_eq!(effect_call(&command, None).unwrap(), None);
}

#[test]
fn in_an_observer_run_task_create_takes_only_the_data() {
    let call = effect_call(
        &task_command(&["create", "--data", r#"{"key": "ABC-1"}"#]),
        Some(RUN),
    )
    .unwrap();

    assert_eq!(
        call,
        Some(EffectCall::CreateTask(ObserverCreateTaskRequest {
            data: json!({"key": "ABC-1"}),
            ..Default::default()
        }))
    );
}

#[test]
fn in_an_observer_run_task_create_may_name_the_workflow_and_repositories() {
    let call = effect_call(
        &task_command(&[
            "create",
            "bugfix",
            "--repository",
            "code=api",
            "--branch",
            "code=main",
            "--data",
            "{}",
        ]),
        Some(RUN),
    )
    .unwrap();

    let Some(EffectCall::CreateTask(request)) = call else {
        panic!("expected a create effect, got {call:?}");
    };
    assert_eq!(request.workflow_id.as_deref(), Some("bugfix"));
    let code = &request.workspaces["code"];
    assert_eq!(code.repository.as_deref(), Some("api"));
    assert_eq!(code.branch.as_deref(), Some("main"));
}

#[test]
fn in_an_observer_run_task_create_refuses_inputs_a_backend_and_commits() {
    for (arguments, refused) in [
        (&["create", "--data", "{}", "--input", "a=b"][..], "--input"),
        (
            &["create", "--data", "{}", "--backend", "k8s"][..],
            "--backend",
        ),
        (
            &["create", "--data", "{}", "--commit", "code=abc"][..],
            "--commit",
        ),
    ] {
        let message = usage_error(arguments, Some(RUN));
        assert!(message.contains(refused), "{refused}: {message}");
        assert!(message.contains("takes only WORKFLOW"), "{message}");
    }
}

#[test]
fn in_an_observer_run_task_create_needs_json_data() {
    let message = usage_error(&["create", "--data", "{oops"], Some(RUN));
    assert!(message.contains("--data"), "{message}");
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
