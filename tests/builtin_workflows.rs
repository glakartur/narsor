//! Tests for the shipped workflows and the search path that reaches them (U12).
//!
//! Two halves:
//!
//! * the resolver itself, driven through the library, where the user directory
//!   is handed in explicitly
//!   ([`WorkflowResolver::with_user_dir`](narsor_engine::workflows::WorkflowResolver::with_user_dir));
//! * the binary, driven through `std::process::Command`, where
//!   `XDG_CONFIG_HOME` points at a temporary directory.
//!
//! # Nothing is written into the repository, or into the real `~/.config`
//!
//! Every test owns a temporary directory and everything it touches lives under
//! it: the store, `{root}/workflows`, and the "user" configuration directory.
//! No test lets the resolver read the environment as it finds it — the library
//! tests pass the directory in, and the process tests set `XDG_CONFIG_HOME` and
//! `HOME` on the child. `workflow eject` without `--to` writes into that
//! directory and nowhere else.
//!
//! # The assertions about the built-ins are made against the embedded strings
//!
//! Deliberately not against `workflows/*.yaml`: a broken `include_str!` must
//! not be able to pass by having the test read the file instead. The one test
//! that opens the files compares them byte for byte with what is embedded, so
//! the two cannot drift.

mod engine_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use narsor_engine::domain::ActionConfig;
use narsor_engine::engine::actions::agent::output_contract;
use narsor_engine::workflows::{
    builtin, builtin_names, WorkflowOrigin, WorkflowResolver, BUILTIN_WORKFLOWS,
};

/// The exit code of a successful command.
const EXIT_SUCCESS: i32 = 0;

/// The exit code of a validation or execution failure.
const EXIT_FAILURE: i32 = 1;

/// The exit code of a wrong invocation.
const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-workflows-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// A subdirectory of the temporary root, created on demand.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create a directory under the temp root");
        path
    }

    /// The store root the binary is pointed at.
    fn root(&self) -> PathBuf {
        self.dir("root")
    }

    /// The stand-in for `$XDG_CONFIG_HOME`.
    fn config(&self) -> PathBuf {
        self.dir("config")
    }

    /// `{root}/workflows`.
    fn root_workflows(&self) -> PathBuf {
        self.dir("root/workflows")
    }

    /// The user's workflow directory inside the stand-in config directory.
    fn user_workflows(&self) -> PathBuf {
        self.dir("config/narsor/workflows")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A resolver over a temporary root whose user directory is the temporary one.
///
/// The environment is never consulted, so the developer's own `~/.config` is
/// out of reach by construction.
fn resolver(root: &TempRoot) -> WorkflowResolver {
    WorkflowResolver::with_user_dir(root.root(), Some(root.user_workflows()))
}

/// Writes a definition into a directory and returns its path.
fn write_workflow(directory: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("create the workflow directory");
    let path = directory.join(name);
    fs::write(&path, body).expect("write the workflow");
    path
}

/// A minimal valid definition, marked so a test can tell copies apart.
fn marked_definition(marker: &str) -> String {
    format!(
        "name: {marker}\n\
         version: 1\n\
         description: A stand-in definition named {marker}.\n\
         states:\n\
         \x20 done:\n\
         \x20   description: Nothing to do.\n\
         terminal_states:\n\
         \x20 - done\n"
    )
}

/// What one invocation of the binary produced.
struct Invocation {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Invocation {
    /// Both streams together, for assertions that do not care which one carried
    /// the text.
    fn output(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

/// Runs the built binary against the temporary root.
///
/// `XDG_CONFIG_HOME` **and** `HOME` are redirected into the temporary
/// directory, so neither the resolver's primary nor its fallback can reach the
/// developer's own configuration.
fn cli(root: &TempRoot, args: &[&str]) -> Invocation {
    cli_with_path(root, args, None)
}

/// The same, with a directory prepended to `PATH` — for the fake `claude`.
fn cli_with_path(root: &TempRoot, args: &[&str], bin: Option<&Path>) -> Invocation {
    let mut command = Command::new(env!("CARGO_BIN_EXE_narsor-cli"));
    command
        .current_dir(root.path())
        .env("XDG_CONFIG_HOME", root.config())
        .env("HOME", root.path())
        .env("NARSOR_LOCAL_WORKSPACE_ROOTS", root.path())
        .arg("--root")
        .arg(root.root())
        .args(args);

    // The daemon a run spawns runs the conductor at every human gate: on
    // the fake harness, never a real `claude`.
    let inherited = engine_support::fake_claude_first_on_path();
    match bin {
        Some(bin) => command.env("PATH", format!("{}:{inherited}", bin.display())),
        None => command.env("PATH", inherited),
    };

    let output = command.output().expect("run the narsor binary");
    Invocation {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// Pulls the execution id out of what `run` printed.
fn execution_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Created execution: "))
        .unwrap_or_else(|| panic!("`run` did not print an execution id; stdout was:\n{stdout}"))
        .trim()
        .to_string()
}

/// The state directory of the runner storage that holds `execution_id`.
///
/// Tasks admitted through the scheduler keep their executions in the local
/// backend's per-task storage, not in the daemon's own `executions` directory.
fn runner_state_dir(root: &TempRoot, execution_id: &str) -> PathBuf {
    let storage = root.root().join("local-controller/storage");
    fs::read_dir(&storage)
        .expect("local task storage")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("state"))
        .find(|state| state.join("executions").join(execution_id).is_dir())
        .unwrap_or_else(|| panic!("no runner storage contains execution {execution_id}"))
}

/// The launch spec sealed for the task that owns `execution_id`.
fn sealed_spec(root: &TempRoot, execution_id: &str) -> serde_json::Value {
    let specs = runner_state_dir(root, execution_id).join("launch-specs");
    let path = fs::read_dir(specs)
        .expect("runner launch specs")
        .find_map(Result::ok)
        .expect("one sealed spec")
        .path();
    let text = fs::read_to_string(path).expect("read the sealed spec");
    serde_json::from_str(&text).expect("parse the sealed spec")
}

/// The execution state a runner recorded, as raw text.
fn stored_state(root: &TempRoot, execution_id: &str) -> String {
    fs::read_to_string(
        runner_state_dir(root, execution_id)
            .join("executions")
            .join(execution_id)
            .join("state.json"),
    )
    .expect("read the stored execution")
}

// ---------------------------------------------------------------------------
// the built-ins themselves
// ---------------------------------------------------------------------------

/// Both shipped workflows are embedded, compile, and have no violations.
///
/// Asserted against the embedded strings: this is the copy an installed binary
/// runs, and a broken `include_str!` must fail here rather than be papered over
/// by reading the file.
#[test]
fn every_builtin_is_embedded_and_validates_without_a_single_violation() {
    assert_eq!(
        builtin_names(),
        vec!["compound-engineering", "open-mercato-sdlc"],
        "both shipped workflows are embedded"
    );

    for entry in BUILTIN_WORKFLOWS {
        assert!(
            !entry.source.trim().is_empty(),
            "the embedded '{}' is not empty",
            entry.name
        );

        let violations = narsor_engine::workflow::validate_source(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' compiles: {error}", entry.name));
        assert!(
            violations.is_empty(),
            "the embedded '{}' has no violations, found: {:?}",
            entry.name,
            violations
        );

        let definition = narsor_engine::workflow::load_from_str(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' loads: {error}", entry.name));
        assert_eq!(
            definition.name, entry.name,
            "the embedded definition is the one it is filed under"
        );
        let display = definition
            .display
            .as_ref()
            .unwrap_or_else(|| panic!("the embedded '{}' declares a display", entry.name));
        assert!(display.title.is_some(), "'{}' has a title", entry.name);
        assert!(!display.links.is_empty(), "'{}' has a link", entry.name);
        assert!(
            !definition.states.is_empty(),
            "the embedded '{}' declares states",
            entry.name
        );
    }
}

/// Shipped workflows deliberately inherit the daemon's chosen default rather
/// than pinning every agent action to Claude Code.  That keeps the workflows
/// portable while explicit `harness:` in a user workflow remains an override.
#[test]
fn builtin_agent_actions_inherit_the_operator_default_harness() {
    for entry in BUILTIN_WORKFLOWS {
        let definition = narsor_engine::workflow::load_from_str(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' loads: {error}", entry.name));
        for (state_id, state) in definition.states {
            let Some(ActionConfig::Agent(action)) = state.action else {
                continue;
            };
            assert!(
                action.harness.is_none(),
                "built-in '{}' state '{state_id}' must inherit the operator default",
                entry.name
            );
        }
    }
}

/// Every label of every state — agent, human and script alike — says what it
/// means: the description is what the agent's contract, the input form and
/// the transition menu show next to the label.
#[test]
fn every_builtin_label_has_a_description() {
    for entry in BUILTIN_WORKFLOWS {
        let definition = narsor_engine::workflow::load_from_str(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' loads: {error}", entry.name));
        for (state_id, state) in &definition.states {
            for output in &state.outputs {
                let described = output
                    .description
                    .as_deref()
                    .is_some_and(|description| !description.trim().is_empty());
                assert!(
                    described,
                    "built-in '{}' state '{state_id}': label '{}' has no description",
                    entry.name, output.label
                );
            }
        }
    }
}

/// AE15: the `work` step no longer lists its labels by hand; the contract the
/// engine appends names `needs_human_decision` together with its description.
#[test]
fn the_work_prompt_leaves_the_label_list_to_the_engine_contract() {
    let entry = builtin("compound-engineering").expect("compound-engineering is built in");
    let definition = narsor_engine::workflow::load_from_str(entry.source).expect("it loads");
    let work = &definition.states["work"];
    let Some(ActionConfig::Agent(action)) = &work.action else {
        panic!("'work' is an agent state");
    };

    assert!(
        !action.prompt.contains("Use exactly one of these labels"),
        "the prompt of 'work' still carries a hand-written label list"
    );

    let needs_human_decision = work
        .outputs
        .iter()
        .find(|output| output.label == "needs_human_decision")
        .expect("'work' declares needs_human_decision");
    let description = needs_human_decision
        .description
        .as_deref()
        .expect("needs_human_decision is described");
    let contract = output_contract(&work.outputs);
    assert!(
        contract.contains(&format!("- `needs_human_decision` — {description}")),
        "the engine's contract lists needs_human_decision with its description: {contract}"
    );
}

/// The embedded text is the file in `workflows/`, byte for byte.
#[test]
fn the_embedded_text_cannot_drift_from_the_repository_files() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
    for entry in BUILTIN_WORKFLOWS {
        let path = directory.join(format!("{}.yaml", entry.name));
        let on_disk = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read '{}': {error}", path.display()));
        assert_eq!(
            entry.source,
            on_disk,
            "the embedded '{}' is byte-identical to {}",
            entry.name,
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// precedence
// ---------------------------------------------------------------------------

/// A bare name reaches the built-in, and so does the same name with a suffix.
#[test]
fn a_bare_name_and_a_suffixed_name_resolve_to_the_same_builtin() {
    let root = TempRoot::new();
    let resolver = resolver(&root);

    let bare = resolver
        .resolve("compound-engineering")
        .expect("a bare name resolves");
    assert_eq!(bare.origin, WorkflowOrigin::BuiltIn);
    assert_eq!(bare.name, "compound-engineering");
    assert_eq!(
        bare.source,
        builtin("compound-engineering")
            .expect("the built-in exists")
            .source,
        "it is the embedded copy that resolves"
    );
    assert!(bare.path().is_none(), "a built-in has no path");

    for suffixed in ["compound-engineering.yaml", "compound-engineering.yml"] {
        let resolved = resolver
            .resolve(suffixed)
            .expect("a suffixed name resolves");
        assert_eq!(resolved, bare, "'{suffixed}' resolves identically");
    }
}

/// An existing file always wins, even when a built-in carries the same name.
#[test]
fn a_file_path_wins_over_a_builtin_of_the_same_name() {
    let root = TempRoot::new();
    let local = write_workflow(
        &root.dir("drafts"),
        "compound-engineering.yaml",
        &marked_definition("from-the-file"),
    );

    let resolved = resolver(&root)
        .resolve(&local.display().to_string())
        .expect("the file resolves");

    assert_eq!(resolved.origin.label(), "file");
    assert_eq!(
        resolved.path(),
        Some(local.canonicalize().unwrap().as_path())
    );
    assert!(
        resolved.source.contains("from-the-file"),
        "the file's own text is what was read"
    );
}

/// `{root}/workflows/<name>.yaml` shadows the built-in of that name.
#[test]
fn the_root_directory_shadows_the_builtin() {
    let root = TempRoot::new();
    write_workflow(
        &root.root_workflows(),
        "compound-engineering.yaml",
        &marked_definition("from-the-root"),
    );

    let resolved = resolver(&root)
        .resolve("compound-engineering")
        .expect("the name resolves");

    assert_eq!(resolved.origin.label(), "root");
    assert!(resolved.source.contains("from-the-root"));
}

/// The user directory shadows the built-in, and loses to `{root}/workflows`.
#[test]
fn the_user_directory_shadows_the_builtin_but_loses_to_the_root() {
    let root = TempRoot::new();
    write_workflow(
        &root.user_workflows(),
        "compound-engineering.yaml",
        &marked_definition("from-the-user-directory"),
    );

    let resolved = resolver(&root)
        .resolve("compound-engineering")
        .expect("the name resolves");
    assert_eq!(resolved.origin.label(), "user");
    assert!(resolved.source.contains("from-the-user-directory"));

    write_workflow(
        &root.root_workflows(),
        "compound-engineering.yaml",
        &marked_definition("from-the-root"),
    );

    let resolved = resolver(&root)
        .resolve("compound-engineering")
        .expect("the name still resolves");
    assert_eq!(
        resolved.origin.label(),
        "root",
        "the data directory has precedence over the user directory"
    );
    assert!(resolved.source.contains("from-the-root"));
}

/// An unknown name says where it looked and what is always available.
#[test]
fn an_unknown_name_names_every_place_it_looked_and_the_available_builtins() {
    let root = TempRoot::new();
    let error = resolver(&root)
        .resolve("no-such-workflow")
        .expect_err("nothing matches");
    let message = error.to_string();

    assert!(
        message.contains("no-such-workflow"),
        "the message repeats what was asked for: {message}"
    );
    assert!(
        message.contains(&root.root_workflows().display().to_string())
            && message.contains(&root.user_workflows().display().to_string()),
        "the message names both search directories: {message}"
    );
    for name in builtin_names() {
        assert!(
            message.contains(name),
            "the message names the built-in '{name}': {message}"
        );
    }
}

/// The listing carries one row per name, with the shadowed copies on it.
#[test]
fn the_listing_reports_an_override_rather_than_two_rows() {
    let root = TempRoot::new();
    let resolver = resolver(&root);

    let entries = resolver.list();
    assert_eq!(
        entries.len(),
        BUILTIN_WORKFLOWS.len(),
        "with nothing installed the listing is exactly the built-ins: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.origin == WorkflowOrigin::BuiltIn && !entry.is_override()),
        "nothing overrides anything yet: {entries:?}"
    );
    assert!(
        entries.iter().all(|entry| entry.description.is_some()),
        "each row carries the definition's description: {entries:?}"
    );

    write_workflow(
        &root.root_workflows(),
        "compound-engineering.yaml",
        &marked_definition("from-the-root"),
    );

    let entries = resolver.list();
    assert_eq!(
        entries.len(),
        BUILTIN_WORKFLOWS.len(),
        "the override is the same row, not a second one: {entries:?}"
    );
    let overridden = entries
        .iter()
        .find(|entry| entry.name == "compound-engineering")
        .expect("the overridden workflow is listed");
    assert_eq!(overridden.origin.label(), "root");
    assert!(overridden.is_override());
    assert_eq!(overridden.shadow_labels(), "built-in");
}

// ---------------------------------------------------------------------------
// the workflow subcommand
// ---------------------------------------------------------------------------

/// `workflow list` shows the built-ins, and marks a copy that shadows one.
#[test]
fn workflow_list_shows_the_builtins_and_marks_an_override() {
    let root = TempRoot::new();

    let listing = cli(&root, &["workflow", "list"]);
    assert_eq!(listing.code, EXIT_SUCCESS, "stderr: {}", listing.stderr);
    for name in builtin_names() {
        assert!(
            listing.stdout.contains(name),
            "'{name}' is listed:\n{}",
            listing.stdout
        );
    }
    assert!(
        listing.stdout.contains("built-in"),
        "the source is shown:\n{}",
        listing.stdout
    );
    assert!(
        !listing.stdout.contains("overrides:"),
        "nothing overrides anything yet:\n{}",
        listing.stdout
    );

    write_workflow(
        &root.root_workflows(),
        "compound-engineering.yaml",
        &marked_definition("from-the-root"),
    );

    let listing = cli(&root, &["workflow", "list"]);
    assert!(
        listing.stdout.contains("overrides: built-in"),
        "the copy is reported as an override:\n{}",
        listing.stdout
    );
    assert!(
        listing.stdout.contains("open-mercato-sdlc"),
        "the other built-in is still listed:\n{}",
        listing.stdout
    );
}

/// `workflow show` prints the YAML on stdout and the source on stderr.
#[test]
fn workflow_show_prints_the_definition_and_says_where_it_came_from() {
    let root = TempRoot::new();

    let shown = cli(&root, &["workflow", "show", "open-mercato-sdlc"]);
    assert_eq!(shown.code, EXIT_SUCCESS, "stderr: {}", shown.stderr);
    assert_eq!(
        shown.stdout,
        builtin("open-mercato-sdlc")
            .expect("the built-in exists")
            .source,
        "stdout is the definition and nothing else, so it can be piped into a file"
    );
    assert!(
        shown.stderr.contains("built-in"),
        "where it came from is said on stderr: {}",
        shown.stderr
    );
}

/// `workflow eject` writes the built-in out, refuses to clobber it, and the
/// copy then takes precedence.
#[test]
fn workflow_eject_writes_refuses_a_second_time_and_then_takes_precedence() {
    let root = TempRoot::new();

    let ejected = cli(&root, &["workflow", "eject", "compound-engineering"]);
    assert_eq!(ejected.code, EXIT_SUCCESS, "stderr: {}", ejected.stderr);

    let target = root
        .config()
        .join("narsor/workflows/compound-engineering.yaml");
    assert!(
        target.is_file(),
        "the file was written to {}",
        target.display()
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read the ejected copy"),
        builtin("compound-engineering")
            .expect("the built-in exists")
            .source,
        "the ejected copy is the built-in, byte for byte"
    );
    assert!(
        ejected.stdout.contains(&target.display().to_string()),
        "the path written is printed:\n{}",
        ejected.stdout
    );
    assert!(
        ejected.stdout.contains("now resolves to this copy"),
        "it says the copy has precedence:\n{}",
        ejected.stdout
    );

    // Nothing outside the temporary directory was touched.
    assert!(
        target.starts_with(root.path()),
        "the default target is the redirected config directory"
    );

    let again = cli(&root, &["workflow", "eject", "compound-engineering"]);
    assert_eq!(
        again.code,
        EXIT_FAILURE,
        "a second eject is refused: {}",
        again.output()
    );
    assert!(
        again.stderr.contains("--force"),
        "the refusal says how to overwrite: {}",
        again.stderr
    );

    // The ejected copy is now what resolves.
    fs::write(&target, marked_definition("from-the-ejected-copy")).expect("edit the ejected copy");
    let shown = cli(&root, &["workflow", "show", "compound-engineering"]);
    assert!(
        shown.stdout.contains("from-the-ejected-copy"),
        "the edited copy is what would run:\n{}",
        shown.stdout
    );
    assert!(
        shown.stderr.contains("user"),
        "and it is reported as coming from the user directory: {}",
        shown.stderr
    );

    let forced = cli(
        &root,
        &["workflow", "eject", "compound-engineering", "--force"],
    );
    assert_eq!(forced.code, EXIT_SUCCESS, "stderr: {}", forced.stderr);
    assert_eq!(
        fs::read_to_string(&target).expect("read the restored copy"),
        builtin("compound-engineering")
            .expect("the built-in exists")
            .source,
        "--force restores the built-in's text"
    );
}

/// `workflow eject --to` writes where it is told, and refuses an unknown name.
#[test]
fn workflow_eject_honours_to_and_refuses_a_name_that_is_not_built_in() {
    let root = TempRoot::new();
    let target_dir = root.path().join("elsewhere/nested");

    let ejected = cli(
        &root,
        &[
            "workflow",
            "eject",
            "open-mercato-sdlc",
            "--to",
            &target_dir.display().to_string(),
        ],
    );
    assert_eq!(ejected.code, EXIT_SUCCESS, "stderr: {}", ejected.stderr);
    assert!(
        target_dir.join("open-mercato-sdlc.yaml").is_file(),
        "the directory was created and the file written"
    );

    let refused = cli(&root, &["workflow", "eject", "not-a-builtin"]);
    assert_eq!(refused.code, EXIT_USAGE, "output: {}", refused.output());
    for name in builtin_names() {
        assert!(
            refused.stderr.contains(name),
            "the refusal names the built-in '{name}': {}",
            refused.stderr
        );
    }
}

/// An unknown workflow is a usage error that names the built-ins.
#[test]
fn running_an_unknown_name_is_a_usage_error_that_names_the_builtins() {
    let root = TempRoot::new();

    let invocation = cli(&root, &["run", "no-such-workflow"]);
    assert_eq!(
        invocation.code,
        EXIT_USAGE,
        "output: {}",
        invocation.output()
    );
    for name in builtin_names() {
        assert!(
            invocation.stderr.contains(name),
            "the message names the built-in '{name}': {}",
            invocation.stderr
        );
    }
    assert!(
        invocation.stderr.contains("Looked for"),
        "the message says where it looked: {}",
        invocation.stderr
    );
}

// ---------------------------------------------------------------------------
// the trap: an execution started from a built-in has no definition path
// ---------------------------------------------------------------------------

/// Installs a `claude` that answers with one fixed label, and nothing else.
///
/// Every state of the shipped workflows that the test reaches is an agent
/// action on the `claude-code` harness, which runs `claude` from `PATH`. The
/// real one costs money and needs credentials, so the test puts its own on the
/// front of `PATH`.
fn install_fake_claude(root: &TempRoot, label: &str) -> PathBuf {
    let bin = root.dir("bin");
    let program = bin.join("claude");
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\
         \"session_id\":\"fake\",\"result\":\"Done.\\n{{\\\"output\\\": \\\"{label}\\\"}}\"}}'\n"
    );
    fs::write(&program, script).expect("write the fake harness");
    make_executable(&program);
    bin
}

/// A local git repository the workspace of a shipped workflow can be cloned
/// from.
fn source_repo(root: &TempRoot) -> PathBuf {
    let path = root.dir("source-repo");
    git(&path, &["init", "-q", "-b", "main", "."]);
    fs::write(path.join("README.md"), "a repository to clone\n").expect("write a file");
    git(&path, &["add", "."]);
    git(
        &path,
        &[
            "-c",
            "user.email=tests@example.test",
            "-c",
            "user.name=tests",
            "commit",
            "-qm",
            "initial",
        ],
    );
    path
}

/// Runs `git` inside a directory, failing loudly.
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Makes a file executable, which the fake harness has to be.
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).expect("stat the file").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make the file executable");
    }
}

/// The test this whole change exists for: an execution started from a built-in
/// **by name** is resumable from a fresh process.
///
/// A built-in has no file, so nothing can be recorded in
/// `metadata.cli.definition_path`; the workflow *name* is recorded instead, and
/// `narsor input` re-resolves from it. Both shipped workflows are full of
/// human gates, so an execution that could not be answered would be useless.
#[test]
#[ignore = "starts a narsor daemon; run with --ignored"]
fn an_execution_started_from_a_builtin_by_name_is_resumable_in_a_fresh_process() {
    let root = TempRoot::new();
    let bin = install_fake_claude(&root, "needs_direction");
    let repo = source_repo(&root);

    let started = cli_with_path(
        &root,
        &[
            "run",
            "compound-engineering",
            "--input",
            "feature_description=a thing worth building",
            "--input",
            &format!("target_repo={}", repo.display()),
        ],
        Some(&bin),
    );
    assert_eq!(
        started.code,
        EXIT_SUCCESS,
        "the built-in ran: {}",
        started.output()
    );
    assert!(
        started.stderr.contains("built-in"),
        "it says the definition came from the built-in: {}",
        started.stderr
    );
    let id = execution_id(&started.stdout);
    assert!(
        started.stdout.contains("Human input expected"),
        "the run stopped on a human gate:\n{}",
        started.stdout
    );

    // What was sealed: the workflow identity, and no path — because there is none.
    assert_eq!(
        sealed_spec(&root, &id)["workflow_id"],
        "compound-engineering",
        "the sealed spec names the workflow"
    );
    assert!(
        !stored_state(&root, &id).contains("definition_path"),
        "a built-in records no path, because it has none"
    );

    // A fresh process: it has to re-resolve the definition from that name.
    let answered = cli_with_path(
        &root,
        &[
            "input",
            &id,
            "--field",
            "direction_decision=abandon",
            "--field",
            "direction_note=not now",
        ],
        Some(&bin),
    );
    assert_eq!(
        answered.code,
        EXIT_SUCCESS,
        "the human gate was answered: {}",
        answered.output()
    );
    assert!(
        answered.stdout.contains("Input delivered to execution"),
        "the input reached the execution:\n{}",
        answered.stdout
    );
    assert!(
        !answered.stdout.contains("clarify_direction"),
        "the execution moved off the gate:\n{}",
        answered.stdout
    );
}

/// The same, for an execution started from a file path: the old behaviour is
/// untouched.
#[test]
#[ignore = "starts a narsor daemon; run with --ignored"]
fn an_execution_started_from_a_file_path_is_still_resumable_in_a_fresh_process() {
    let root = TempRoot::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cli");
    let definition = fixtures.join("greet.yaml");

    let started = cli(
        &root,
        &[
            "run",
            &definition.display().to_string(),
            "--input",
            "greeting=Good morning",
            "--input",
            &format!("script_path={}", fixtures.join("announce.sh").display()),
        ],
    );
    assert_eq!(
        started.code,
        EXIT_SUCCESS,
        "the file ran: {}",
        started.output()
    );
    let id = execution_id(&started.stdout);

    assert!(
        !stored_state(&root, &id).contains("definition_path"),
        "runner executions rely on the sealed definition, not a mutable path"
    );
    assert!(
        sealed_spec(&root, &id)["definition"]["id"].is_string(),
        "the definition travels inside the sealed spec"
    );

    let answered = cli(
        &root,
        &["input", &id, "--field", "approval_status=approved"],
    );
    assert_eq!(
        answered.code,
        EXIT_SUCCESS,
        "the question was answered: {}",
        answered.output()
    );
    assert!(
        answered.stdout.contains("Status: completed"),
        "the execution finished:\n{}",
        answered.stdout
    );
}
