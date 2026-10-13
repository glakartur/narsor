use std::process::Command;

#[test]
fn client_package_does_not_depend_on_process_implementations() {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ])
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let packages = metadata["packages"].as_array().expect("packages array");
    let client = packages
        .iter()
        .find(|package| package["name"] == "narsor")
        .expect("client package");
    let client_id = client["id"].as_str().expect("client package id");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolved dependency nodes");
    let by_id = nodes
        .iter()
        .map(|node| (node["id"].as_str().expect("node package id"), node))
        .collect::<std::collections::HashMap<_, _>>();
    let forbidden = [
        "narsor-controller",
        "narsor-engine",
        "narsor-runner",
        "narsor-server",
    ];
    let mut pending = vec![client_id];
    let mut visited = std::collections::HashSet::new();
    let mut found = std::collections::BTreeSet::new();

    while let Some(package_id) = pending.pop() {
        if !visited.insert(package_id) {
            continue;
        }
        let Some(node) = by_id.get(package_id) else {
            continue;
        };
        for dependency in node["deps"].as_array().expect("node dependencies") {
            let normal_dependency = dependency["dep_kinds"]
                .as_array()
                .expect("dependency kinds")
                .iter()
                .any(|kind| kind["kind"].is_null());
            if !normal_dependency {
                continue;
            }
            let dependency_id = dependency["pkg"].as_str().expect("dependency package id");
            if let Some(package) = packages
                .iter()
                .find(|package| package["id"] == dependency_id)
            {
                let name = package["name"].as_str().expect("dependency package name");
                if forbidden.contains(&name) {
                    found.insert(name.to_owned());
                }
            }
            pending.push(dependency_id);
        }
    }

    assert!(
        found.is_empty(),
        "client normal dependency graph includes process implementations: {found:?}"
    );
}

#[test]
fn controller_runtime_does_not_depend_on_engine() {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ])
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let packages = metadata["packages"].as_array().expect("packages array");
    let runtime = packages
        .iter()
        .find(|package| package["name"] == "narsor-controller-runtime")
        .expect("runtime package");
    let runtime_id = runtime["id"].as_str().expect("runtime package id");
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolved dependency nodes");
    let by_id = nodes
        .iter()
        .map(|node| (node["id"].as_str().expect("node package id"), node))
        .collect::<std::collections::HashMap<_, _>>();
    let mut pending = vec![runtime_id];
    let mut visited = std::collections::HashSet::new();
    while let Some(package_id) = pending.pop() {
        if !visited.insert(package_id) {
            continue;
        }
        let Some(node) = by_id.get(package_id) else {
            continue;
        };
        for dependency in node["deps"].as_array().expect("node dependencies") {
            let normal = dependency["dep_kinds"]
                .as_array()
                .expect("dependency kinds")
                .iter()
                .any(|kind| kind["kind"].is_null());
            if !normal {
                continue;
            }
            let dependency_id = dependency["pkg"].as_str().expect("dependency package id");
            let package = packages
                .iter()
                .find(|package| package["id"] == dependency_id)
                .expect("dependency package");
            assert_ne!(
                package["name"], "narsor-engine",
                "controller-runtime normal dependencies include narsor-engine"
            );
            pending.push(dependency_id);
        }
    }
}

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

#[test]
fn cli_cannot_start_the_server_in_process() {
    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .arg("daemon")
        .output()
        .expect("run narsor-cli daemon without an operation");
    let error = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(
        error.contains("Usage: narsor-cli daemon [OPTIONS] <COMMAND>"),
        "{error}"
    );
    assert!(error.contains("stop"), "{error}");

    let output = Command::new(env!("CARGO_BIN_EXE_narsor-cli"))
        .args(["daemon", "start"])
        .output()
        .expect("run narsor-cli daemon start");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand 'start'"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
