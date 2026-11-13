//! The request `narsor task create` builds from its arguments, and what
//! `task create --effect-key` and `task event` ask for in an observer's
//! script (`NARSOR_OBSERVER_RUN`, passed in rather than set).

use clap::Parser;
use narsor::cli::observer_effect::{effect_call, EffectCall};
use narsor::cli::remote::{create_request, TaskCommand};
use narsor::cli::{Cli, Command};
use narsor_protocol::controller::{
    CreateTaskRequest, ObserverCreateTaskRequest, ObserverEventRequest,
};
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
fn an_effect_key_or_a_task_event_outside_an_observer_run_is_a_usage_error() {
    let message = usage_error(&["create", "--effect-key", "ABC-1"], None);
    assert!(message.contains("--effect-key"), "{message}");
    assert!(message.contains("NARSOR_OBSERVER_RUN"), "{message}");

    let message = usage_error(
        &["event", "task-1", "--type", "approved", "--key", "MR!1"],
        None,
    );
    assert!(message.contains("task event"), "{message}");
    assert!(message.contains("NARSOR_OBSERVER_RUN"), "{message}");

    // An empty variable is no run either.
    usage_error(&["create", "--effect-key", "ABC-1"], Some(""));
}

#[test]
fn outside_an_observer_run_task_create_goes_its_usual_way() {
    let command = task_command(&["create", "flow", "--input", "code=acme"]);

    assert_eq!(effect_call(&command, None).unwrap(), None);
}

#[test]
fn in_an_observer_run_task_create_takes_only_the_key_and_the_data() {
    let call = effect_call(
        &task_command(&[
            "create",
            "--effect-key",
            "ABC-1",
            "--data",
            r#"{"key": "ABC-1"}"#,
        ]),
        Some(RUN),
    )
    .unwrap();

    assert_eq!(
        call,
        Some(EffectCall::CreateTask(ObserverCreateTaskRequest {
            effect_key: "ABC-1".into(),
            data: json!({"key": "ABC-1"}),
        }))
    );
}

#[test]
fn in_an_observer_run_task_create_refuses_a_workflow_inputs_a_backend_and_repositories() {
    for (arguments, refused) in [
        (&["create", "flow", "--effect-key", "K"][..], "WORKFLOW"),
        (
            &["create", "--effect-key", "K", "--input", "a=b"][..],
            "--input",
        ),
        (
            &["create", "--effect-key", "K", "--backend", "k8s"][..],
            "--backend",
        ),
        (
            &["create", "--effect-key", "K", "--repository", "code=acme"][..],
            "--repository",
        ),
    ] {
        let message = usage_error(arguments, Some(RUN));
        assert!(message.contains(refused), "{refused}: {message}");
        assert!(
            message.contains("only --effect-key and --data"),
            "{message}"
        );
    }

    let message = usage_error(&["create", "flow"], Some(RUN));
    assert!(message.contains("WORKFLOW"), "{message}");
}

#[test]
fn in_an_observer_run_task_create_needs_an_effect_key_and_json_data() {
    let message = usage_error(
        &["create", "--effect-key", "K", "--data", "{oops"],
        Some(RUN),
    );
    assert!(message.contains("--data"), "{message}");

    let no_key = Cli::try_parse_from(["narsor-cli", "task", "create", "--data", "{}"]);
    assert!(no_key.is_err(), "--data needs --effect-key");
}

#[test]
fn in_an_observer_run_task_event_builds_the_event() {
    let call = effect_call(
        &task_command(&[
            "event",
            "task-1",
            "--type",
            "approved",
            "--key",
            "MR!17:approved",
            "--data",
            r#"{"mr": 17}"#,
        ]),
        Some(RUN),
    )
    .unwrap();

    assert_eq!(
        call,
        Some(EffectCall::Event {
            task_id: "task-1".into(),
            request: ObserverEventRequest {
                effect_key: "MR!17:approved".into(),
                event_type: "approved".into(),
                data: json!({"mr": 17}),
            },
        })
    );
}
