//! Tests for the shipped workflows and the search path that reaches them (U12).
//!
//! The tests drive the resolver and the embedded definitions through the
//! library. The user directory is handed in explicitly
//! ([`WorkflowResolver::with_user_dir`](narsor_engine::workflows::WorkflowResolver::with_user_dir)).
//! No test starts a daemon or a process.
//!
//! # Nothing is written into the repository, or into the real `~/.config`
//!
//! Every test owns a temporary directory and everything it touches lives under
//! it: the store, `{root}/workflows`, and the "user" configuration directory.
//! No test lets the resolver read the environment as it finds it.
//!
//! # The assertions about the built-ins are made against the embedded strings
//!
//! Deliberately not against `workflows/*.yaml`: a broken `include_str!` must
//! not be able to pass by having the test read the file instead. The one test
//! that opens the files compares them byte for byte with what is embedded, so
//! the two cannot drift.

use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::domain::ActionConfig;
use narsor_engine::engine::actions::agent::output_contract;
use narsor_engine::engine::StepContract;
use narsor_engine::workflows::{
    builtin, builtin_names, WorkflowOrigin, WorkflowResolver, BUILTIN_WORKFLOWS,
};

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

/// A shipped workflow must not name a branch: the repository configuration
/// decides which one is fetched, and `HEAD` is the fallback.
#[test]
fn no_builtin_pins_a_branch_for_its_workspace() {
    for entry in BUILTIN_WORKFLOWS {
        let definition = narsor_engine::workflow::load_from_str(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' loads: {error}", entry.name));
        match definition.workspace {
            Some(narsor_engine::domain::WorkspaceSpec::Git { git_ref, .. }) => {
                assert_eq!(git_ref, None, "'{}' names no ref", entry.name)
            }
            other => panic!("'{}' has a git workspace, found {other:?}", entry.name),
        }
    }
}

/// A builtin names no `ref`, so the `default_branch` of the registered
/// repository decides which branch its workspace checks out.
#[test]
fn a_builtin_workspace_checks_out_the_registered_default_branch() {
    let root = TempRoot::new();
    narsor_engine::registry::RepositoryRegistry::new(root.path())
        .add(
            narsor_engine::registry::RepositoryEntry::new(
                "acme-api",
                "https://example.test/acme/api.git",
            )
            .with_default_branch("develop"),
        )
        .expect("register the repository");
    for entry in BUILTIN_WORKFLOWS {
        let definition = narsor_engine::workflow::load_from_str(entry.source)
            .unwrap_or_else(|error| panic!("the embedded '{}' loads: {error}", entry.name));
        let Some(narsor_engine::domain::WorkspaceSpec::Git { git_ref, .. }) = definition.workspace
        else {
            panic!("'{}' has a git workspace", entry.name);
        };
        let spec =
            narsor_engine::tasks::resolve_repository(root.path(), "acme-api", git_ref.as_deref())
                .expect("resolve the repository");
        match spec.source {
            narsor_engine::workspace::RepositorySource::Git { git_ref, .. } => assert_eq!(
                git_ref.as_deref(),
                Some("develop"),
                "'{}' follows the registry",
                entry.name
            ),
            other => panic!("expected a git repository, got {other:?}"),
        }
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
            for output in &state.choices {
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

/// The compound engineering workflow, compiled from its embedded text.
fn compound_engineering() -> narsor_engine::domain::WorkflowDefinition {
    let entry = builtin("compound-engineering").expect("compound-engineering is built in");
    narsor_engine::workflow::load_from_str(entry.source).expect("it loads")
}

/// The choice `label` of `state`.
fn choice<'a>(
    definition: &'a narsor_engine::domain::WorkflowDefinition,
    state: &str,
    label: &str,
) -> &'a narsor_engine::domain::ChoiceDefinition {
    definition.states[state]
        .choices
        .iter()
        .find(|choice| choice.label == label)
        .unwrap_or_else(|| panic!("'{state}' has no choice '{label}'"))
}

/// The repair loops have limits, and only they do (KTD15).
#[test]
fn compound_engineering_limits_its_repair_loops() {
    let definition = compound_engineering();
    let limits: Vec<(&str, u32)> = definition
        .states
        .iter()
        .filter_map(|(id, state)| state.max_entries.map(|limit| (id.as_str(), limit)))
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect();
    assert_eq!(
        limits,
        vec![
            ("code_review", 10),
            ("repair_merge", 3),
            ("repair_verification", 5),
        ]
    );
}

/// Each choice requires what the table of step contracts in the plan says:
/// variables and artifacts by name, each one defined on its step.
#[test]
fn compound_engineering_choices_require_their_variables_and_artifacts() {
    let definition = compound_engineering();
    let requires = |state: &str, label: &str| {
        let choice = choice(&definition, state, label);
        (
            choice.requires_variables.clone(),
            choice.requires_artifacts.clone(),
        )
    };
    let variables = |names: &[&str]| -> (Vec<String>, Vec<String>) {
        (names.iter().map(ToString::to_string).collect(), Vec::new())
    };
    let artifacts = |names: &[&str]| -> (Vec<String>, Vec<String>) {
        (Vec::new(), names.iter().map(ToString::to_string).collect())
    };

    assert_eq!(
        requires("brainstorm", "requirements_drafted"),
        artifacts(&["requirements_doc"])
    );
    assert_eq!(
        requires("brainstorm", "requirements_ready"),
        (
            vec!["review_summary".to_string()],
            vec!["requirements_doc".to_string()]
        )
    );
    for review in ["doc_review", "plan_review"] {
        assert_eq!(requires(review, "reviewed"), variables(&["review_summary"]));
        assert!(
            !choice(&definition, review, "reviewed").requires_human_approval,
            "the result of '{review}' goes to the merge request without a stop"
        );
    }
    for publish in [
        "publish_requirements",
        "publish_plan",
        "publish_work",
        "publish_learning",
    ] {
        assert_eq!(requires(publish, "published"), variables(&["pr_url"]));
    }
    assert_eq!(requires("plan", "plan_ready"), artifacts(&["plan_doc"]));
    assert_eq!(
        requires("plan", "needs_requirements_revision"),
        variables(&["requirements_blocker"])
    );
    assert_eq!(requires("work", "implemented"), variables(&["test_baseline"]));
    assert_eq!(
        requires("work", "needs_plan_revision"),
        variables(&["plan_blocker"])
    );
    assert_eq!(requires("compound", "documented"), artifacts(&["learning_doc"]));
}

/// `brainstorm` has two ways out and never asks the person; the review result
/// reaches the merge request through the publish steps.
#[test]
fn brainstorm_does_not_ask_and_can_skip_the_review() {
    let definition = compound_engineering();
    let brainstorm = &definition.states["brainstorm"];
    assert_eq!(brainstorm.transitions["requirements_drafted"], "doc_review");
    assert_eq!(
        brainstorm.transitions["requirements_ready"],
        "publish_requirements"
    );
    assert!(
        brainstorm
            .variables
            .iter()
            .any(|variable| variable.name == "review_summary"),
        "'brainstorm' defines the reason for skipping the review"
    );
    assert!(
        choice(&definition, "brainstorm", "requirements_drafted")
            .requires_variables
            .is_empty()
    );

    let Some(ActionConfig::Agent(action)) = &brainstorm.action else {
        panic!("'brainstorm' is an agent step");
    };
    assert!(!action.prompt.contains("ask the person;"));
    assert!(action.prompt.contains("Do not ask the person anything"));
    assert!(action.prompt.contains("Założenia do potwierdzenia"));
    assert!(action
        .prompt
        .contains("Pytania otwarte dla użytkownika"));

    for publish in ["publish_requirements", "publish_plan"] {
        let Some(ActionConfig::Agent(action)) = &definition.states[publish].action else {
            panic!("'{publish}' is an agent step");
        };
        assert!(
            action.prompt.contains("{{ context.review_summary }}"),
            "'{publish}' puts the review summary on the merge request"
        );
    }
}

/// No step of `compound-engineering` stops for a person before publishing.
#[test]
fn compound_engineering_has_no_choice_that_requires_human_approval() {
    let definition = compound_engineering();
    for (state_id, state) in &definition.states {
        for choice in &state.choices {
            assert!(
                !choice.requires_human_approval,
                "'{state_id}' choice '{}' stops for a person",
                choice.label
            );
        }
    }
}

/// A step that reads a blocker also defines it, so it can clear it (KTD6).
#[test]
fn compound_engineering_steps_that_read_a_blocker_can_clear_it() {
    let definition = compound_engineering();
    for (state, blocker) in [
        ("brainstorm", "requirements_blocker"),
        ("plan", "plan_blocker"),
    ] {
        let Some(ActionConfig::Agent(action)) = &definition.states[state].action else {
            panic!("'{state}' is an agent step");
        };
        assert!(action.prompt.contains(&format!("{{{{ context.{blocker} }}}}")));
        assert!(
            definition.states[state]
                .variables
                .iter()
                .any(|variable| variable.name == blocker),
            "'{state}' reads '{blocker}' and must define it to clear it"
        );
    }
    for (state, document) in [("plan", "plan_doc"), ("compound", "learning_doc")] {
        let Some(ActionConfig::Agent(action)) = &definition.states[state].action else {
            panic!("'{state}' is an agent step");
        };
        assert!(
            action
                .prompt
                .contains(&format!("{{{{ artifacts.{document} }}}}")),
            "'{state}' revises its own document on a later pass"
        );
    }
}

/// Every `revise` at a gate continues the session of the stage's author with
/// the comment (R26).
#[test]
fn compound_engineering_revisions_continue_the_author_session() {
    let definition = compound_engineering();
    for (gate, label) in [
        ("approve_requirements", "revise"),
        ("approve_plan", "revise"),
        ("approve_plan", "revise_requirements"),
        ("approve_work", "revise"),
        ("approve_work", "revise_plan"),
        ("approve_learning", "revise"),
    ] {
        assert!(
            choice(&definition, gate, label)
                .resume_comment_field
                .is_some(),
            "'{gate}' choice '{label}' must carry the comment to the author"
        );
    }
}

/// `finalize` has its own time limit and a `timeout` choice, because the
/// engine kills it on the limit and its own clean-up never runs (KTD14).
#[test]
fn compound_engineering_finalize_is_a_script_with_a_timeout_choice() {
    let definition = compound_engineering();
    for step in ["finalize", "disarm_merge"] {
        let Some(ActionConfig::Script(script)) = &definition.states[step].action else {
            panic!("'{step}' is a script step");
        };
        assert!(script.args.iter().any(|arg| arg.contains("glab")));
        assert!(
            !script.args.iter().any(|arg| arg
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == "main")),
            "'{step}' takes the target branch from the merge request, not a literal"
        );
    }
    let finalize = &definition.states["finalize"];
    assert_eq!(finalize.timeout_secs, Some(70 * 60));
    assert_eq!(finalize.transitions["timeout"], "disarm_merge");
}

/// The engine appends the reply contract to every agent prompt, so no
/// open-mercato prompt carries a hand-written JSON format of its own. The PR
/// the display shows is a declared variable of `open_pr`; everything else an
/// agent reports goes to `additional_data`.
#[test]
fn open_mercato_prompts_leave_the_reply_format_to_the_engine() {
    let entry = builtin("open-mercato-sdlc").expect("open-mercato-sdlc is built in");
    let definition = narsor_engine::workflow::load_from_str(entry.source).expect("it loads");

    let mut agent_states = 0;
    for (id, state) in &definition.states {
        let Some(ActionConfig::Agent(action)) = &state.action else {
            continue;
        };
        agent_states += 1;
        for old_format in ["\"output\"", "\"data\"", "awaiting_input"] {
            assert!(
                !action.prompt.contains(old_format),
                "the prompt of '{id}' still carries `{old_format}`, which the engine's \
                 contract covers"
            );
        }
    }
    assert_eq!(agent_states, 15, "open-mercato has fifteen agent steps");

    let declared: Vec<&str> = definition.states["open_pr"]
        .variables
        .iter()
        .map(|variable| variable.name.as_str())
        .collect();
    assert_eq!(declared, vec!["pr_number", "pr_url"]);
    for (id, state) in &definition.states {
        if id != "open_pr" {
            assert!(
                state.variables.is_empty() && state.artifacts.is_empty(),
                "only 'open_pr' sets a value a later step or the display reads; '{id}' \
                 declares one"
            );
        }
    }
}

/// The implementation agent asks questions through `awaiting_input`, then
/// resumes the same action; it does not route ambiguity through a workflow gate.
/// The engine's output contract teaches `awaiting_input`, so the prompt leaves
/// it to the contract instead of repeating it.
#[test]
fn the_work_prompt_uses_agent_input_for_ambiguity() {
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

    assert!(
        !action.prompt.contains("awaiting_input"),
        "the prompt of 'work' repeats what the engine's output contract says"
    );
    assert!(
        output_contract(&StepContract::of(work)).contains("awaiting_input"),
        "the output contract tells the agent to ask and resume in place"
    );
    let labels: Vec<&str> = work
        .choices
        .iter()
        .map(|choice| choice.label.as_str())
        .collect();
    assert_eq!(labels, vec!["implemented", "needs_plan_revision"]);
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
