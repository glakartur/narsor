use std::process::Command;

#[test]
fn controller_binary_exposes_its_process_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-controller"))
        .arg("--help")
        .output()
        .expect("run narsor-controller --help");
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(help.contains("Reconcile one Narsor backend"));
    assert!(help.contains("--control-plane"));
    assert!(help.contains("--kubernetes-config"));
}

#[test]
fn controller_binary_reports_missing_required_options_as_usage_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-controller"))
        .output()
        .expect("run narsor-controller without options");
    let error = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(error.contains("--control-plane"), "{error}");
    assert!(error.contains("--credential-file"), "{error}");
    assert!(error.contains("--kubernetes-config"), "{error}");
}
