//! The request `narsor task create` builds from its arguments.

use clap::Parser;
use narsor::cli::remote::create_request;
use narsor::cli::{Cli, Command};
use narsor_protocol::controller::CreateTaskRequest;

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
