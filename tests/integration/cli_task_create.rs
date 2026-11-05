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
fn a_branch_names_the_input_it_is_for() {
    let request = request(&["flow", "--input", "code=acme", "--branch", "code=develop"]);

    assert_eq!(request.launch.branches.len(), 1);
    assert_eq!(request.launch.branches["code"], "develop");
}

#[test]
fn two_branches_for_two_inputs_are_both_sent() {
    let request = request(&[
        "flow",
        "--branch",
        "code=develop",
        "--branch",
        "docs=release/1.2",
    ]);

    assert_eq!(request.launch.branches["code"], "develop");
    assert_eq!(request.launch.branches["docs"], "release/1.2");
}

#[test]
fn a_branch_that_reads_as_json_stays_text() {
    let request = request(&["flow", "--branch", "code=123", "--branch", "docs=true"]);

    assert_eq!(request.launch.branches["code"], "123");
    assert_eq!(request.launch.branches["docs"], "true");
}

#[test]
fn without_a_branch_the_request_has_no_branches_key() {
    let request = request(&["flow", "--input", "code=acme"]);

    let json = serde_json::to_value(&request.launch).expect("serializes");
    assert!(json.get("branches").is_none(), "{json}");
}

#[test]
fn a_branch_without_an_input_name_is_a_usage_error_with_the_format() {
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
fn a_branch_does_not_go_with_a_definition_file() {
    let error = parse(&["--file", "flow.yaml", "--branch", "code=develop"])
        .expect_err("a file carries no branch");

    assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
}
