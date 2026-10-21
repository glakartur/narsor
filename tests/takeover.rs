//! AE1: taking an execution over is the same operation as recovering one.
//!
//! The proof is an equivalence: a run that is never interrupted and a run that
//! is interrupted after step k and taken over by a fresh process must reach the
//! same final state, for every k. Script actions only, so the comparison is
//! deterministic.

mod engine_support;

use engine_support::{workflow, Harness};
use narsor_engine::domain::ExecutionStatus;

#[tokio::test]
async fn an_interrupted_run_ends_where_an_uninterrupted_one_does() {
    let definition = workflow("three_steps");

    let reference = {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();
        engine
            .start_and_run(&definition, serde_json::json!({}), harness.workspace())
            .await
            .expect("reference run")
    };
    assert_eq!(
        reference.status,
        ExecutionStatus::Completed,
        "reference run"
    );

    for interrupt_after in 1..=3 {
        let harness = Harness::new();
        let engine = harness.engine().with_checkpoints();

        let execution = engine
            .start(&definition, serde_json::json!({}), harness.workspace())
            .expect("start");

        let execution = engine
            .run_steps(&definition, &execution.id, interrupt_after)
            .await
            .expect("run up to the interruption");
        assert_eq!(
            execution.status,
            ExecutionStatus::Running,
            "the run must be left mid-workflow after step {interrupt_after}"
        );

        // The process dies here: a fresh store, a fresh engine, and a work tree
        // that is deliberately destroyed so nothing can be recovered from it.
        std::fs::remove_dir_all(&execution.workspace.path).expect("destroy the work tree");

        let taken_over =
            narsor_engine::tasks::takeover(&harness.store(), &harness.provisioner(), &execution.id)
                .expect("take the execution over");

        let finished = harness
            .engine()
            .with_checkpoints()
            .run(&definition, &taken_over.id)
            .await
            .expect("finish after the takeover");

        assert_eq!(
            finished.status,
            ExecutionStatus::Completed,
            "takeover after step {interrupt_after} must finish the workflow"
        );
        assert_eq!(
            finished.context.variables, reference.context.variables,
            "takeover after step {interrupt_after} must reach the reference context"
        );
        assert_eq!(
            finished.current_state, reference.current_state,
            "takeover after step {interrupt_after} must reach the reference state"
        );

        // AE9: the handover mechanism pushes nothing to the product repository.
        assert!(
            harness.origin_refs().is_empty(),
            "takeover after step {interrupt_after} must leave the product repo untouched, found: {:?}",
            harness.origin_refs()
        );
    }
}

// ---------------------------------------------------------------------------
// A git-cloned workspace: the delta is measured against the pinned base commit
// ---------------------------------------------------------------------------

/// Three script steps over a git workspace: leave an untracked file, delete a
/// tracked one, then report what the tree holds. The last step is the witness —
/// its variables only match the reference when the restored tree is exactly the
/// one the run left behind, with nothing resurrected and nothing leaked in.
const GIT_WORKFLOW: &str = r#"
name: three-steps-over-git
version: 1
description: >
  Three script states over a cloned repository, written into the test's
  temporary root. Used to prove that a takeover restores a git workspace
  against the commit it was pinned to.

inputs:
  - name: repository
    type: string
    required: true
    description: The origin the workspace is cloned from.

globals:
  workspace:
    type: git
    repository: "{{ inputs.repository }}"
    ref: main

states:
  first:
    action:
      type: script
      command: ./record_step.sh
      args:
        - first
    choices:
      - done
    transitions:
      done: second

  second:
    action:
      type: script
      command: ./drop.sh
      args:
        - doomed.txt
    choices:
      - done
    transitions:
      done: third

  third:
    action:
      type: script
      command: ./list_tree.sh
    choices:
      - done
    transitions:
      done: completed

  completed:
    description: Every step went through.

terminal_states:
  - completed
"#;

/// Deletes the file named by the first argument.
const DROP_SCRIPT: &str = r#"#!/bin/sh
rm -f "$1"
printf '{"output": "done", "variables": {"dropped": "%s"}}\n' "$1"
"#;

/// Reports every file of the work tree (outside `.git`) and the README text.
const LIST_TREE_SCRIPT: &str = r#"#!/bin/sh
tree=$(find . -path ./.git -prune -o -type f -print | sed 's|^\./||' | sort | tr '\n' ' ' | sed 's/ $//')
readme=$(cat README.md)
printf '{"output": "done", "variables": {"tree": "%s", "readme": "%s"}}\n' "$tree" "$readme"
"#;

/// Runs `git` in `dir` with a fixed identity and returns its trimmed stdout.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Takeover Test"])
        .args(["-c", "user.email=takeover@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Writes an executable script.
fn write_script(path: &std::path::Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
}

/// A git product repository for one harness: a seed clone to commit from and a
/// bare origin, at `Harness::origin_dir`, that the workspace is cloned from.
struct Origin {
    seed: std::path::PathBuf,
    bare: std::path::PathBuf,
}

impl Origin {
    /// An origin whose `main` holds the scripts, a README and `doomed.txt`.
    fn new(harness: &Harness) -> Self {
        let seed = harness.root().join("seed");
        std::fs::create_dir_all(&seed).expect("create the seed repository");
        git(&seed, &["-c", "init.defaultBranch=main", "init", "--quiet"]);
        std::fs::copy(
            engine_support::fixtures().join("scripts/record_step.sh"),
            seed.join("record_step.sh"),
        )
        .expect("copy record_step.sh");
        write_script(&seed.join("drop.sh"), DROP_SCRIPT);
        write_script(&seed.join("list_tree.sh"), LIST_TREE_SCRIPT);
        std::fs::write(seed.join("README.md"), "base\n").expect("write README");
        std::fs::write(seed.join("doomed.txt"), "tracked, then deleted\n").expect("write doomed");
        git(&seed, &["add", "--all"]);
        git(&seed, &["commit", "--quiet", "--message", "base"]);

        let bare = harness.origin_dir();
        let bare_arg = bare.display().to_string();
        git(&seed, &["clone", "--quiet", "--bare", ".", &bare_arg]);
        git(&seed, &["remote", "add", "origin", &bare_arg]);
        Self { seed, bare }
    }

    /// The commit `main` points at in the origin.
    fn main_commit(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    /// Moves `main` on upstream while the run is parked: a new commit that adds
    /// a file and rewrites both the README and the file the run deletes.
    fn drift(&self) -> String {
        std::fs::write(self.seed.join("upstream.txt"), "added upstream\n").expect("write upstream");
        std::fs::write(self.seed.join("README.md"), "drifted\n").expect("rewrite README");
        std::fs::write(self.seed.join("doomed.txt"), "rewritten upstream\n")
            .expect("rewrite doomed");
        git(&self.seed, &["add", "--all"]);
        git(
            &self.seed,
            &["commit", "--quiet", "--message", "upstream drift"],
        );
        git(&self.seed, &["push", "--quiet", "origin", "main"]);
        self.main_commit()
    }
}

/// Writes the git workflow into the harness root and loads it.
fn git_workflow(
    harness: &Harness,
) -> (
    narsor_engine::domain::WorkflowDefinition,
    std::path::PathBuf,
) {
    let path = harness.root().join("three-steps-over-git.yaml");
    std::fs::write(&path, GIT_WORKFLOW).expect("write the git workflow");
    let definition = narsor_engine::workflow::load_from_file(&path).expect("load the git workflow");
    (definition, path)
}

/// Starts the git workflow in `harness` the way `narsor run` does, so the
/// workspace is provisioned — and its base commit recorded — by production code.
fn start_git_task(
    harness: &Harness,
    definition: &narsor_engine::domain::WorkflowDefinition,
    file: &std::path::Path,
    engine: &narsor_engine::engine::Engine,
) -> narsor_engine::domain::WorkflowExecution {
    let catalog = harness.root().join("catalog");
    std::fs::create_dir_all(&catalog).expect("create the catalogue");
    let mut inputs = serde_json::Map::new();
    inputs.insert(
        "repository".to_string(),
        serde_json::Value::String(harness.origin_dir().display().to_string()),
    );
    narsor_engine::tasks::start_task(
        &catalog,
        &harness.root().join("work"),
        engine,
        definition,
        file,
        inputs,
    )
    .expect("start the git task")
}

/// The variables the steps produced, without the `repository` input — which
/// names each harness's own temporary origin and so differs between runs.
fn produced_variables(
    execution: &narsor_engine::domain::WorkflowExecution,
) -> serde_json::Map<String, serde_json::Value> {
    let mut variables: serde_json::Map<String, serde_json::Value> = execution
        .context
        .variables
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    variables.remove("repository");
    variables
}

/// The refs a bare origin with one branch lists: nothing was pushed to it.
fn untouched_origin_refs() -> Vec<String> {
    vec!["HEAD".to_string(), "refs/heads/main".to_string()]
}

#[tokio::test]
async fn a_taken_over_git_workspace_is_restored_against_its_pinned_base() {
    let reference = {
        let harness = Harness::new();
        let _origin = Origin::new(&harness);
        let (definition, file) = git_workflow(&harness);
        let engine = harness.engine().with_checkpoints();
        let execution = start_git_task(&harness, &definition, &file, &engine);
        let finished = engine
            .run(&definition, &execution.id)
            .await
            .expect("reference run");
        assert_eq!(harness.origin_refs(), untouched_origin_refs());
        finished
    };
    assert_eq!(
        reference.status,
        ExecutionStatus::Completed,
        "reference run"
    );
    let reference_tree = reference
        .context
        .variables
        .get("tree")
        .and_then(serde_json::Value::as_str)
        .expect("the reference run reports its tree")
        .to_string();
    assert!(
        !reference_tree.contains("doomed.txt"),
        "the reference run deleted doomed.txt: {reference_tree}"
    );

    for interrupt_after in 1..=3 {
        let harness = Harness::new();
        let origin = Origin::new(&harness);
        let base = origin.main_commit();
        let (definition, file) = git_workflow(&harness);
        let engine = harness.engine().with_checkpoints();

        let execution = start_git_task(&harness, &definition, &file, &engine);
        assert_eq!(
            execution.workspace.base_commit.as_deref(),
            Some(base.as_str()),
            "the workspace pins the commit it was cloned at"
        );

        let execution = engine
            .run_steps(&definition, &execution.id, interrupt_after)
            .await
            .expect("run up to the interruption");
        assert_eq!(
            execution.status,
            ExecutionStatus::Running,
            "the run must be left mid-workflow after step {interrupt_after}"
        );

        // The process dies, its work tree with it, and upstream moves on while
        // nobody is running the execution.
        std::fs::remove_dir_all(&execution.workspace.path).expect("destroy the work tree");
        let drifted = origin.drift();
        assert_ne!(drifted, base);

        let taken_over =
            narsor_engine::tasks::takeover(&harness.store(), &harness.provisioner(), &execution.id)
                .expect("take the execution over");

        let tree = std::path::PathBuf::from(&taken_over.workspace.path);
        assert_eq!(
            git(&tree, &["rev-parse", "HEAD"]),
            base,
            "takeover after step {interrupt_after} must sit on the pinned base, not the moved branch"
        );
        assert!(
            !tree.join("upstream.txt").exists(),
            "takeover after step {interrupt_after} leaked a file added upstream"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join("README.md")).expect("read README"),
            "base\n",
            "takeover after step {interrupt_after} leaked an upstream edit"
        );
        assert!(
            tree.join("first.step").is_file(),
            "takeover after step {interrupt_after} lost the untracked file of step one"
        );
        assert_eq!(
            tree.join("doomed.txt").exists(),
            interrupt_after < 2,
            "takeover after step {interrupt_after}: doomed.txt must exist exactly until step two deletes it"
        );

        let finished = harness
            .engine()
            .with_checkpoints()
            .run(&definition, &taken_over.id)
            .await
            .expect("finish after the takeover");

        assert_eq!(
            finished.status,
            ExecutionStatus::Completed,
            "takeover after step {interrupt_after} must finish the workflow"
        );
        assert_eq!(
            produced_variables(&finished),
            produced_variables(&reference),
            "takeover after step {interrupt_after} must reach the reference context"
        );
        assert_eq!(
            finished.current_state, reference.current_state,
            "takeover after step {interrupt_after} must reach the reference state"
        );

        // AE9: the handover pushed nothing — the origin holds exactly the refs
        // it had, and `main` is still the upstream commit, not anything the
        // run produced.
        assert_eq!(
            harness.origin_refs(),
            untouched_origin_refs(),
            "takeover after step {interrupt_after} must not add refs to the product repo"
        );
        assert_eq!(
            origin.main_commit(),
            drifted,
            "takeover after step {interrupt_after} must not move the product repo's main"
        );
    }
}
