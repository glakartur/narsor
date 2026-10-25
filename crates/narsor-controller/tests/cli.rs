use std::process::Command;

#[test]
fn controller_binary_exposes_its_process_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-controller"))
        .args(["kubernetes", "--help"])
        .output()
        .expect("run narsor-controller kubernetes --help");
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(help.contains("--control-plane"));
    assert!(help.contains("--kubernetes-config"));
}

#[test]
fn controller_binary_reports_missing_required_options_as_usage_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-controller"))
        .arg("kubernetes")
        .env_remove("NARSOR_CONTROL_PLANE")
        .output()
        .expect("run narsor-controller kubernetes without options");
    let error = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(error.contains("--control-plane"), "{error}");
}

#[test]
fn controller_binary_requires_a_backend() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-controller"))
        .output()
        .expect("run narsor-controller without a backend");
    let error = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(error.contains("kubernetes"), "{error}");
    assert!(error.contains("local"), "{error}");
}
