use std::process::Command;

#[test]
fn narsor_is_the_tui_entry_point() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor"))
        .arg("--help")
        .output()
        .expect("run narsor help");
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(help.contains("Narsor terminal interface"));
    assert!(!help.contains("Commands:"));
    assert!(!help.contains("\n  validate"));
}

#[test]
fn narsor_cli_has_the_operator_command_set() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .arg("--help")
        .output()
        .expect("run narsor-cli help");
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(help.contains("Usage: narsor-cli"));
    assert!(help.contains("Commands:"));
    assert!(help.contains("validate"));
    assert!(help.contains("task"));
    assert!(!help.contains("\n  tui"));
    assert!(!help.contains("\n  runner"));
}
