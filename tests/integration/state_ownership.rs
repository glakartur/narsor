//! The state ownership rule (D82; R6, R9 of the controller-protocol plan):
//! every binary owns `state/<binary>/` and `work/<binary>/`, and only its own
//! crate may build the typed paths of those directories.
//!
//! Two checks over the non-test sources of every workspace crate:
//!
//! 1. A path type is constructed (`Type::new(`) only in the crates of
//!    [`CONSTRUCTORS`].
//! 2. The raw root accessors (`.state_dir()`, `.work_dir()`) are called only
//!    on a line that also constructs a path type the crate may construct, or
//!    where [`RAW_ROOT_USES`] allows it with a reason.
//!
//! This is a text scan, not a compiler pass. What counts as a test source:
//! files below a `tests/` directory, files named `tests.rs`, `*_tests.rs` or
//! `test_support.rs`, and the item that follows `#[cfg(test)]` (a block up to
//! its closing brace, counted naively). Full-line comments are skipped. A
//! type imported under another name escapes the scan.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Which crate may construct which path type (KTD9).
const CONSTRUCTORS: &[(&str, &[&str])] = &[
    // The server creates `state/run/` and writes the discovery file; the
    // client library discovers or autostarts the server there. The CLI, the
    // TUI and the web UI reach it only through `narsor-api-client`.
    ("RunPaths", &["narsor-server", "narsor-api-client"]),
    // Engine stores take it as a parameter; only the server builds it.
    // `narsor-migrate` moves an old installation into the server's and the
    // local controller's directories once, so it builds their path types
    // (D82, KTD10); it is the one program that knows both layouts.
    ("ServerStatePaths", &["narsor-server", "narsor-migrate"]),
    (
        "ControllerStatePaths",
        &["narsor-controller", "narsor-migrate"],
    ),
    (
        "ControllerWorkPaths",
        &["narsor-controller", "narsor-migrate"],
    ),
    ("WebStatePaths", &["narsor-web"]),
    ("TuiStatePaths", &["narsor"]),
];

/// The raw root accessors.
const RAW_ROOTS: &[&str] = &[".state_dir()", ".work_dir()"];

/// A raw root call that is allowed although it constructs nothing on its line.
struct RawRootUse {
    /// Path of the source file, relative to the workspace root.
    file: &'static str,
    /// Text of the call on that line.
    call: &'static str,
    reason: &'static str,
}

const RAW_ROOT_USES: &[RawRootUse] = &[
    RawRootUse {
        file: "crates/narsor-shared/src/config/old_layout.rs",
        call: "config.state_dir()",
        reason: "the old layout is computed from the roots, for narsor-migrate and the \
                 server's refusal to start on it (R6 c)",
    },
    RawRootUse {
        file: "crates/narsor-shared/src/config/old_layout.rs",
        call: "config.work_dir()",
        reason: "the old layout is computed from the roots, for narsor-migrate and the \
                 server's refusal to start on it (R6 c)",
    },
    RawRootUse {
        file: "crates/narsor-migrate/src/lib.rs",
        call: "(config.state_dir(), config.work_dir())",
        reason: "narsor-migrate builds the server's and the local controller's path types \
                 from the roots to move an old installation into them (KTD10)",
    },
    RawRootUse {
        file: "crates/narsor-server/src/runtime.rs",
        call: "state_root: config.state_dir()",
        reason: "delegation: the server passes the roots to the local controller (KTD7)",
    },
    RawRootUse {
        file: "crates/narsor-server/src/runtime.rs",
        call: "work_root: config.work_dir()",
        reason: "delegation: the server passes the roots to the local controller (KTD7)",
    },
    RawRootUse {
        file: "crates/narsor-web/src/main.rs",
        call: "resolved.state_dir()",
        reason: "builds WebStatePaths and hands the root to the client library's discovery",
    },
    RawRootUse {
        file: "src/cli/mod.rs",
        call: "config.state_dir()",
        reason: "handed to discovery (RunPaths in narsor-api-client) and as --state-root \
                 to controller helper commands (KTD7)",
    },
    RawRootUse {
        file: "src/cli/commands/tui.rs",
        call: "config.state_dir()",
        reason: "handed to discovery (RunPaths in narsor-api-client)",
    },
    RawRootUse {
        file: "src/cli/commands/task_control.rs",
        call: "daemon_client(config.state_dir()",
        reason: "handed to discovery (RunPaths in narsor-api-client)",
    },
    RawRootUse {
        file: "src/cli/commands/workflow.rs",
        call: "daemon_client(config.state_dir()",
        reason: "handed to discovery (RunPaths in narsor-api-client)",
    },
    RawRootUse {
        file: "crates/narsor-runner/src/runner/storage.rs",
        call: "self.state_dir()",
        reason: "same name, other method: the task storage's own directory, not a root",
    },
    RawRootUse {
        file: "crates/narsor-controller/src/local/container/mod.rs",
        call: "self.store.work_dir()",
        reason: "same name, other method: the controller's work directory, not a root",
    },
];

/// The violations in one source file of `krate`. `used` collects the indexes
/// of the [`RAW_ROOT_USES`] entries that allowed a call.
fn violations(krate: &str, file: &str, source: &str, used: &mut HashSet<usize>) -> Vec<String> {
    let allowed: Vec<&str> = CONSTRUCTORS
        .iter()
        .filter(|(_, crates)| crates.contains(&krate))
        .map(|(ty, _)| *ty)
        .collect();
    let mut found = Vec::new();
    for (number, line) in non_test_lines(source) {
        let constructs = |ty: &str| line.contains(&format!("{ty}::new("));
        for (ty, crates) in CONSTRUCTORS {
            if constructs(ty) && !crates.contains(&krate) {
                found.push(format!("{file}:{number}: {krate} constructs {ty}"));
            }
        }
        let raw = RAW_ROOTS.iter().any(|call| line.contains(call));
        if !raw || allowed.iter().any(|ty| constructs(ty)) {
            continue;
        }
        let entry = RAW_ROOT_USES
            .iter()
            .position(|entry| entry.file == file && line.contains(entry.call));
        match entry {
            Some(index) => {
                used.insert(index);
            }
            None => found.push(format!(
                "{file}:{number}: {krate} uses a raw root accessor: {}",
                line.trim()
            )),
        }
    }
    found
}

/// The lines of `source` outside comments and `#[cfg(test)]` items, numbered
/// from 1.
fn non_test_lines(source: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut after_cfg_test = false;
    let mut depth = 0i64;
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if depth > 0 {
            depth += braces(line);
            continue;
        }
        if trimmed == "#[cfg(test)]" {
            after_cfg_test = true;
            continue;
        }
        if after_cfg_test {
            if trimmed.starts_with("#[") {
                continue;
            }
            after_cfg_test = false;
            depth = braces(line).max(0);
            continue;
        }
        if !trimmed.starts_with("//") {
            lines.push((index + 1, line));
        }
    }
    lines
}

fn braces(line: &str) -> i64 {
    line.chars().fold(0, |depth, c| match c {
        '{' => depth + 1,
        '}' => depth - 1,
        _ => depth,
    })
}

fn is_test_file(path: &Path) -> bool {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    stem == "tests"
        || stem == "test_support"
        || stem.ends_with("_tests")
        || path.components().any(|c| c.as_os_str() == "tests")
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// Every workspace crate as (name, source directory): the root `narsor`
/// crate and each `crates/<name>/src`.
fn workspace_crates(root: &Path) -> Vec<(String, PathBuf)> {
    let mut crates = vec![("narsor".to_string(), root.join("src"))];
    for entry in fs::read_dir(root.join("crates")).unwrap() {
        let dir = entry.unwrap().path();
        if dir.join("src").is_dir() {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            crates.push((name, dir.join("src")));
        }
    }
    crates
}

#[test]
fn the_checker_reports_foreign_paths_and_raw_roots_and_skips_tests() {
    let cases: &[(&str, &str, usize)] = &[
        // A client builds the controller's directory.
        (
            "narsor-api-client",
            "let paths = ControllerStatePaths::new(root);",
            1,
        ),
        // A binary reads the raw root to spell a directory of its own.
        (
            "narsor-web",
            "let dir = resolved.state_dir().join(\"narsor-server\");",
            1,
        ),
        // The raw root builds the crate's own type.
        (
            "narsor-web",
            "let paths = WebStatePaths::new(resolved.state_dir());",
            0,
        ),
        // A test module may build anything.
        (
            "narsor-web",
            "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() {\n        ControllerStatePaths::new(x);\n    }\n}",
            0,
        ),
        // Code after a test module is checked again.
        (
            "narsor-web",
            "#[cfg(test)]\nmod tests {\n}\nfn c() { ServerStatePaths::new(x); }",
            1,
        ),
    ];
    for (krate, source, expected) in cases {
        let found = violations(krate, "src/fixture.rs", source, &mut HashSet::new());
        assert_eq!(found.len(), *expected, "{source}\n{found:?}");
    }
}

#[test]
fn every_crate_builds_only_its_own_paths_and_uses_no_raw_root() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates = workspace_crates(root);
    for (_, owners) in CONSTRUCTORS {
        for owner in *owners {
            assert!(
                crates.iter().any(|(name, _)| name == owner),
                "{owner} is not a workspace crate"
            );
        }
    }

    let mut found = Vec::new();
    let mut used = HashSet::new();
    for (name, dir) in &crates {
        let mut files = Vec::new();
        rust_files(dir, &mut files);
        for path in files.iter().filter(|path| !is_test_file(path)) {
            let file = path.strip_prefix(root).unwrap().to_string_lossy();
            let source = fs::read_to_string(path).unwrap();
            found.extend(violations(name, &file, &source, &mut used));
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));

    let stale: Vec<String> = RAW_ROOT_USES
        .iter()
        .enumerate()
        .filter(|(index, _)| !used.contains(index))
        .map(|(_, entry)| format!("{}: {} ({})", entry.file, entry.call, entry.reason))
        .collect();
    assert!(
        stale.is_empty(),
        "allowlist entries that match nothing:\n{}",
        stale.join("\n")
    );
}
