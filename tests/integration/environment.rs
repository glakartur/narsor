//! Resolving a create-task request into a backend and a pinned environment
//! (U6: R3, R10, R15-R21, R39, R42; KTD5).
//!
//! Every test runs the real [`CatalogLaunchResolver`] against a real
//! catalogue in a temporary directory: the value of this layer is what the
//! files on disk resolve to, and which of them wins.

use crate::environment_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use environment_support::{
    backend, digest, resolver, resolver_with_local_roots, seed_catalog, workflow_image,
    workflow_yaml,
};
use narsor_engine::domain::{
    ImageDigest, ImageSource, ImageSpec, LaunchDefinitionError, RefOrigin, ResolvedEnvironment,
    ResourceProfile, SecretDelivery, TaskLaunchSpec, TaskRepository, TaskRepositorySource,
    TaskWorkspace, TASK_LAUNCH_SPEC_VERSION,
};
use narsor_engine::environment::{
    BackendKind, BackendRecord, EnvironmentCatalog, LaunchError, LaunchIntent, LaunchResolver,
    ProfileRecord, ResolvedLaunch, StaticImageResolver, TrustScope,
};
use narsor_engine::registry::{RepositoryEntry, RepositoryRegistry};

/// A temporary catalogue root that deletes itself.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-env-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        seed_catalog(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn catalog(&self) -> EnvironmentCatalog {
        EnvironmentCatalog::new(&self.path)
    }

    fn registry(&self) -> RepositoryRegistry {
        RepositoryRegistry::new(&self.path)
    }

    fn write_workflow(&self, name: &str, yaml: &str) {
        fs::write(
            self.path.join("workflows").join(format!("{name}.yaml")),
            yaml,
        )
        .expect("write workflow");
    }

    fn resolve(&self, intent: &LaunchIntent) -> Result<ResolvedLaunch, LaunchError> {
        resolver(&self.path).resolve(intent)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn intent(workflow: &str) -> LaunchIntent {
    LaunchIntent {
        workflow_id: workflow.to_string(),
        workflow_version: 1,
        inputs: serde_json::json!({}),
        ..LaunchIntent::default()
    }
}

/// A workflow that works on repository `app` and pins no image itself.
fn repository_workflow(image_line: &str, step_image_line: &str) -> String {
    format!(
        "name: on-app\nversion: 1\nglobals:\n  workspace:\n    type: git\n    repository: app\n\
         {image_line}states:\n  work:\n{step_image_line}    action:\n      type: script\n      \
         command: \"true\"\n    choices: [success]\n    transitions:\n      success: done\n  \
         done:\n    description: finished\nterminal_states: [done]\n"
    )
}

fn pinned(ch: char) -> String {
    format!("ghcr.io/acme/app@{}", digest(ch))
}

fn on_app() -> LaunchIntent {
    LaunchIntent {
        commit: Some(COMMIT.to_string()),
        ..intent("on-app")
    }
}

// ---------------------------------------------------------------------------
// Base image precedence (R15, R18, AE11)
// ---------------------------------------------------------------------------

#[test]
fn a_repository_without_a_base_image_is_refused_before_anything_exists() {
    let root = TempRoot::new();
    root.registry()
        .add(RepositoryEntry::new("app", "https://example.com/app.git"))
        .unwrap();
    root.write_workflow("on-app", &repository_workflow("", ""));

    let error = root.resolve(&on_app()).expect_err("no image anywhere");
    assert!(
        matches!(error, LaunchError::MissingBaseImage { .. }),
        "{error}"
    );
    assert_eq!(error.code(), "missing_base_image");
    assert!(error.to_string().contains("'app'"), "{error}");

    root.registry()
        .update(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_base_image(ImageSpec::pinned(pinned('d'))),
        )
        .unwrap();
    let launch = root.resolve(&on_app()).expect("resolves now");
    let base = launch.spec.environment.base_image.expect("base image");
    assert_eq!(base.digest.as_str(), digest('d'));
    assert_eq!(
        base.source,
        ImageSource::Repository {
            repository: "app".to_string()
        }
    );
    assert!(!base.tag_resolved);
    // A converted `v1` workflow is one primary repository named after the
    // registry id.
    let [repository] = launch.spec.workspace.repositories.as_slice() else {
        panic!("expected one repository, got {:?}", launch.spec.workspace);
    };
    assert_eq!(repository.name, "app");
    let TaskRepositorySource::Git(git) = &repository.source else {
        panic!("expected a git repository, got {repository:?}");
    };
    assert_eq!(git.remote_url, "https://example.com/app.git");
    assert_eq!(git.commit, COMMIT);
    assert_eq!(git.repository.as_deref(), Some("app"));
}

#[test]
fn a_workflow_image_wins_over_the_repository_image() {
    let root = TempRoot::new();
    root.registry()
        .add(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_base_image(ImageSpec::pinned(pinned('d'))),
        )
        .unwrap();
    root.write_workflow(
        "on-app",
        &repository_workflow(&format!("  image: \"{}\"\n", pinned('e')), ""),
    );

    let environment = root.resolve(&on_app()).unwrap().spec.environment;
    let base = environment.base_image.expect("base image");
    assert_eq!(base.digest.as_str(), digest('e'));
    assert_eq!(
        base.source,
        ImageSource::Workflow {
            workflow: "on-app".to_string()
        }
    );
    assert!(environment.step_images.is_empty());
}

#[test]
fn a_step_image_wins_over_the_workflow_and_the_repository() {
    let root = TempRoot::new();
    root.registry()
        .add(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_base_image(ImageSpec::pinned(pinned('d'))),
        )
        .unwrap();
    root.write_workflow(
        "on-app",
        &repository_workflow(
            &format!("  image: \"{}\"\n", pinned('e')),
            &format!("    image: \"{}\"\n", pinned('f')),
        ),
    );

    let environment = root.resolve(&on_app()).unwrap().spec.environment;
    let step = environment.image_for_state("work").expect("step image");
    assert_eq!(step.digest.as_str(), digest('f'));
    assert_eq!(
        step.source,
        ImageSource::Step {
            workflow: "on-app".to_string(),
            state: "work".to_string()
        }
    );
    assert_eq!(
        environment.image_for_state("done").unwrap().digest.as_str(),
        digest('e'),
        "a state without an override runs the workflow image"
    );
}

#[test]
fn a_tag_without_a_digest_is_refused_unless_the_resolver_pins_it() {
    let root = TempRoot::new();
    let tagged = "ghcr.io/acme/dev:2026.10";
    root.write_workflow(
        "tagged",
        &workflow_yaml("tagged", None).replacen(
            "states:",
            &format!(
                "globals:\n  image:\n    reference: {tagged}\n    digest_policy: resolve\nstates:"
            ),
            1,
        ),
    );

    let error = root
        .resolve(&intent("tagged"))
        .expect_err("no tag resolution");
    assert_eq!(error.code(), "unresolved_image", "{error}");

    let images =
        Arc::new(StaticImageResolver::new().with(tagged, ImageDigest::parse(digest('9')).unwrap()));
    let resolve = |images: Arc<StaticImageResolver>| {
        resolver(root.path())
            .with_image_resolver(images)
            .resolve(&intent("tagged"))
    };
    let launch = resolve(Arc::clone(&images)).expect("the tag resolves");
    let base = launch.spec.environment.base_image.clone().unwrap();
    assert!(base.tag_resolved);
    assert_eq!(base.reference, tagged);
    assert_eq!(base.pinned, format!("ghcr.io/acme/dev@{}", digest('9')));

    // Moving the tag later changes new tasks only: the spec already built
    // keeps its digest.
    images.set(tagged, ImageDigest::parse(digest('8')).unwrap());
    let later = resolve(images).unwrap();
    assert_ne!(
        later.spec.environment.identity,
        launch.spec.environment.identity
    );
    assert_eq!(
        launch.spec.environment.base_image.unwrap().digest.as_str(),
        digest('9')
    );

    // Under the default pinned policy a tag is never resolved at all.
    root.write_workflow("tagged", &workflow_yaml("tagged", Some(tagged)));
    let error = root.resolve(&intent("tagged")).expect_err("pinned policy");
    assert_eq!(error.code(), "invalid_workflow", "{error}");
}

#[test]
fn a_local_backend_needs_no_image_and_no_runtime_bundle() {
    let root = TempRoot::new();
    let mut local = BackendRecord::new(
        "local",
        BackendKind::Local,
        TrustScope::Workstation,
        backend("x").limits,
    );
    local.harnesses = Vec::new();
    root.catalog().save_backend(&local).unwrap();
    root.write_workflow("bare", &workflow_yaml("bare", None));

    let launch = root
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            ..intent("bare")
        })
        .expect("local runs without an image");
    assert!(launch.spec.environment.base_image.is_none());
    assert!(launch.spec.environment.runtime_bundle.is_none());

    let error = root.resolve(&intent("bare")).expect_err("k8s needs one");
    assert_eq!(error.code(), "missing_base_image");
}

#[test]
fn a_local_workspace_is_sealed_only_for_the_local_backend() {
    let root = TempRoot::new();
    let source = root.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("README.md"), "local source\n").unwrap();

    let local = BackendRecord::new(
        "local",
        BackendKind::Local,
        TrustScope::Workstation,
        backend("x").limits,
    );
    root.catalog().save_backend(&local).unwrap();
    root.write_workflow(
        "local-folder",
        &format!(
            "name: local-folder\nversion: 1\nglobals:\n  workspace:\n    type: local\n    \
             path: {}\nstates:\n  done:\n    description: done\nterminal_states: [done]\n",
            source.display()
        ),
    );

    let launch = root
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            ..intent("local-folder")
        })
        .expect("local workspace seals on the local backend");
    assert_eq!(
        launch.spec.workspace,
        TaskWorkspace::single(TaskRepository::new(
            "source",
            TaskRepositorySource::Local {
                path: source.canonicalize().unwrap().display().to_string()
            }
        ))
    );

    let error = root
        .resolve(&LaunchIntent {
            backend: Some("k8s".to_string()),
            ..intent("local-folder")
        })
        .expect_err("a host directory cannot travel to k8s");
    assert_eq!(error.code(), "unsupported_workspace");
}

/// A workflow on a local backend whose workspace is `path`.
fn local_folder_task(root: &TempRoot, path: &str) {
    let local = BackendRecord::new(
        "local",
        BackendKind::Local,
        TrustScope::Workstation,
        backend("x").limits,
    );
    root.catalog().save_backend(&local).unwrap();
    root.write_workflow(
        "local-folder",
        &format!(
            "name: local-folder\nversion: 1\nglobals:\n  workspace:\n    type: local\n    \
             path: {path}\nstates:\n  done:\n    description: done\nterminal_states: [done]\n"
        ),
    );
}

fn resolve_local(root: &TempRoot, roots: Vec<PathBuf>) -> Result<ResolvedLaunch, LaunchError> {
    resolver_with_local_roots(&root.path, roots).resolve(&LaunchIntent {
        backend: Some("local".to_string()),
        ..intent("local-folder")
    })
}

#[test]
fn a_local_workspace_must_lie_inside_a_listed_root() {
    let root = TempRoot::new();
    let allowed = root.path.join("allowed");
    let outside = root.path.join("outside");
    fs::create_dir_all(allowed.join("project")).unwrap();
    fs::create_dir_all(&outside).unwrap();

    // Inside a listed root: sealed, in canonical form.
    local_folder_task(&root, &allowed.join("project").display().to_string());
    let launch = resolve_local(&root, vec![allowed.clone()]).expect("inside the root");
    assert_eq!(
        launch.spec.workspace,
        TaskWorkspace::single(TaskRepository::new(
            "project",
            TaskRepositorySource::Local {
                path: allowed
                    .join("project")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string()
            }
        ))
    );

    // The default — no roots — refuses every host directory.
    let refused = resolve_local(&root, Vec::new()).unwrap_err();
    assert_eq!(refused.code(), "unsupported_workspace");
    assert!(refused.to_string().contains("local_workspace_roots"));

    // Outside every listed root.
    local_folder_task(&root, &outside.display().to_string());
    let refused = resolve_local(&root, vec![allowed.clone()]).unwrap_err();
    assert_eq!(refused.code(), "unsupported_workspace");

    // `..` cannot climb out of the root it starts in.
    local_folder_task(
        &root,
        &allowed.join("project/../../outside").display().to_string(),
    );
    let refused = resolve_local(&root, vec![allowed.clone()]).unwrap_err();
    assert_eq!(refused.code(), "unsupported_workspace");

    // Nor can a symlink inside the root lead out of it.
    std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
    local_folder_task(&root, &allowed.join("escape").display().to_string());
    let refused = resolve_local(&root, vec![allowed.clone()]).unwrap_err();
    assert_eq!(refused.code(), "unsupported_workspace");

    // A relative path names nothing definite.
    local_folder_task(&root, "project");
    let refused = resolve_local(&root, vec![allowed]).unwrap_err();
    assert_eq!(refused.code(), "unsupported_workspace");
}

// ---------------------------------------------------------------------------
// Backend selection and limits (R3, R10)
// ---------------------------------------------------------------------------

#[test]
fn the_owner_default_backend_applies_unless_the_request_names_another() {
    let root = TempRoot::new();
    let by_default = root.resolve(&intent("wf")).unwrap();
    assert_eq!(by_default.backend_id.as_str(), "k8s");

    let explicit = root
        .resolve(&LaunchIntent {
            backend: Some("backend-b".to_string()),
            ..intent("wf")
        })
        .unwrap();
    assert_eq!(explicit.backend_id.as_str(), "backend-b");
}

#[test]
fn an_unknown_or_malformed_backend_is_refused() {
    let root = TempRoot::new();
    for (backend, code) in [
        ("nowhere", "unknown_backend"),
        ("../x", "invalid_identifier"),
    ] {
        let error = root
            .resolve(&LaunchIntent {
                backend: Some(backend.to_string()),
                ..intent("wf")
            })
            .expect_err("refused");
        assert_eq!(error.code(), code, "{error}");
    }
}

#[test]
fn invalid_backend_limits_are_refused_with_every_problem() {
    let root = TempRoot::new();
    fs::write(
        root.path().join("backends/k8s.yaml"),
        "kind: kubernetes\ntrust_scope: shared\nlimits:\n  \
         default_resources: { cpu_millicores: 0, memory_mib: 512 }\n  \
         max_resources: { cpu_millicores: 100, memory_mib: 256 }\n",
    )
    .unwrap();

    let error = root.resolve(&intent("wf")).expect_err("refused");
    let LaunchError::InvalidBackend { problems, .. } = &error else {
        panic!("expected invalid backend, got {error}");
    };
    assert_eq!(error.code(), "invalid_backend");
    assert!(
        problems.iter().any(|p| p.contains("cpu_millicores")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("exceed")),
        "{problems:?}"
    );

    // The catalogue refuses to store such a record in the first place.
    let mut bad = backend("k8s");
    bad.limits.slot_release_grace_secs = 0;
    assert!(root.catalog().save_backend(&bad).is_err());
}

#[test]
fn resources_default_to_the_backend_and_never_exceed_its_maximum() {
    let root = TempRoot::new();
    let launch = root.resolve(&intent("wf")).unwrap();
    assert_eq!(launch.spec.resources, ResourceProfile::new(1000, 2048));

    let bigger = LaunchIntent {
        resources: Some(ResourceProfile::new(4000, 8192)),
        ..intent("wf")
    };
    assert_eq!(
        root.resolve(&bigger).unwrap().spec.resources,
        ResourceProfile::new(4000, 8192)
    );

    for resources in [ResourceProfile::new(4001, 1), ResourceProfile::new(0, 1)] {
        let error = root
            .resolve(&LaunchIntent {
                resources: Some(resources),
                ..intent("wf")
            })
            .expect_err("refused");
        assert_eq!(error.code(), "invalid_resources", "{error}");
    }
}

// ---------------------------------------------------------------------------
// Profiles, secrets and harnesses (R16, R19, R42)
// ---------------------------------------------------------------------------

fn profile(secrets: &[&str], tools: &[&str]) -> ProfileRecord {
    ProfileRecord {
        id: "default".to_string(),
        version: 2,
        secrets: secrets.iter().map(ToString::to_string).collect(),
        tools: tools.iter().map(ToString::to_string).collect(),
        skills: vec!["code-review".to_string()],
        ..ProfileRecord::default()
    }
}

#[test]
fn a_profile_adds_tools_and_secret_references_from_the_backend_allow_list() {
    let root = TempRoot::new();
    root.catalog()
        .save_profile(&profile(&["github-token"], &["gh"]))
        .unwrap();

    let environment = root.resolve(&intent("wf")).unwrap().spec.environment;
    assert_eq!(environment.profile.version, 2);
    assert_eq!(environment.profile.skills, vec!["code-review"]);
    assert_eq!(environment.tools["gh"].digest.as_str(), digest('c'));
    assert_eq!(
        environment.runtime_bundle.unwrap().digest.as_str(),
        digest('b')
    );
    assert_eq!(environment.secrets.len(), 1);
    let secret = &environment.secrets[0];
    assert_eq!(secret.name, "github-token");
    assert_eq!(secret.backend.as_str(), "k8s");
    assert_eq!(secret.key, "narsor-github/token");
    assert_eq!(secret.delivery, SecretDelivery::Env("GH_TOKEN".to_string()));
}

#[test]
fn unregistered_secret_references_are_refused() {
    let root = TempRoot::new();
    root.catalog()
        .save_profile(&profile(&["aws-key"], &[]))
        .unwrap();
    let error = root
        .resolve(&intent("wf"))
        .expect_err("not on the allow list");
    assert_eq!(error.code(), "unregistered_secret", "{error}");
    assert!(error.to_string().contains("aws-key"));

    // The same reference is fine on a backend that registers it — and a
    // repository's git credential is held to the same list.
    let root = TempRoot::new();
    root.registry()
        .add(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_base_image(ImageSpec::pinned(pinned('d')))
                .with_git_credential("gitlab-token"),
        )
        .unwrap();
    root.write_workflow("on-app", &repository_workflow("", ""));
    let error = root.resolve(&on_app()).expect_err("unknown credential");
    assert_eq!(error.code(), "unregistered_secret", "{error}");
}

#[test]
fn an_unknown_tool_or_profile_is_refused() {
    let root = TempRoot::new();
    root.catalog()
        .save_profile(&profile(&[], &["kubectl"]))
        .unwrap();
    assert_eq!(
        root.resolve(&intent("wf")).unwrap_err().code(),
        "unknown_tool"
    );
    let error = root
        .resolve(&LaunchIntent {
            profile: Some("someone".to_string()),
            ..intent("wf")
        })
        .unwrap_err();
    assert_eq!(error.code(), "unknown_profile");
}

#[test]
fn an_unsupported_harness_is_refused() {
    let root = TempRoot::new();
    let error = root
        .resolve(&LaunchIntent {
            harness: Some("nonesuch".to_string()),
            ..intent("wf")
        })
        .unwrap_err();
    assert_eq!(error.code(), "unsupported_harness", "{error}");

    let mut restricted = backend("k8s");
    restricted.harnesses = vec!["codex".to_string()];
    root.catalog().save_backend(&restricted).unwrap();
    let error = root.resolve(&intent("wf")).unwrap_err();
    assert_eq!(error.code(), "unsupported_harness", "{error}");
    assert!(error.to_string().contains("codex"), "{error}");

    let pinned = root
        .resolve(&LaunchIntent {
            harness: Some("codex".to_string()),
            ..intent("wf")
        })
        .unwrap();
    assert_eq!(pinned.spec.environment.harness, "codex");
}

// ---------------------------------------------------------------------------
// Git identity (R39)
// ---------------------------------------------------------------------------

#[test]
fn a_local_backend_pins_an_unpinned_workspace_at_admission_not_creation() {
    let root = TempRoot::new();
    root.registry()
        .add(RepositoryEntry::new(
            "app",
            "https://example.invalid/app.git",
        ))
        .unwrap();
    root.write_workflow("on-app", &repository_workflow("", ""));

    // No commit, and a remote that cannot even be reached: creation must not
    // touch the network, so it still succeeds and leaves the commit open.
    let launch = root
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            ..intent("on-app")
        })
        .unwrap();
    let Some(TaskRepositorySource::Git(git)) = launch
        .spec
        .workspace
        .primary()
        .map(|repository| &repository.source)
    else {
        panic!("git workspace expected");
    };
    assert!(!git.is_pinned(), "the commit is resolved at admission");
    assert_eq!(git.git_ref, None);
    assert_eq!(git.remote_url, "https://example.invalid/app.git");

    // An explicit commit still pins at creation.
    let pinned = root
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            ..on_app()
        })
        .unwrap();
    let Some(TaskRepositorySource::Git(git)) = pinned
        .spec
        .workspace
        .primary()
        .map(|repository| &repository.source)
    else {
        panic!("git workspace expected");
    };
    assert!(git.is_pinned());
    assert_eq!(git.commit, COMMIT);
}

/// Resolves `workflow` on the local backend and returns the git identity.
fn resolve_git_on_local(
    root: &TempRoot,
    entry: RepositoryEntry,
    ref_line: &str,
    intent_commit: Option<&str>,
    repository: Option<&str>,
) -> narsor_engine::domain::GitIdentity {
    root.registry().add(entry).unwrap();
    root.write_workflow("on-app", &repository_workflow(ref_line, ""));
    let launch = root
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            commit: intent_commit.map(ToString::to_string),
            repository: repository.map(ToString::to_string),
            ..intent("on-app")
        })
        .unwrap();
    let Some(TaskRepository {
        source: TaskRepositorySource::Git(git),
        ..
    }) = launch.spec.workspace.repositories.into_iter().next()
    else {
        panic!("git workspace expected");
    };
    git
}

fn app_entry() -> RepositoryEntry {
    RepositoryEntry::new("app", "https://example.invalid/app.git")
}

#[test]
fn the_repository_branch_is_used_when_the_workflow_names_no_ref() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(
        &root,
        app_entry().with_default_branch("develop"),
        "",
        None,
        None,
    );
    assert_eq!(git.git_ref.as_deref(), Some("develop"));
    assert_eq!(git.ref_origin, Some(RefOrigin::Repository));
}

#[test]
fn no_branch_and_no_ref_leave_the_remote_head_to_the_clone() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(&root, app_entry(), "", None, None);
    assert_eq!(git.git_ref, None);
    assert_eq!(git.ref_origin, None);
}

#[test]
fn a_blank_repository_branch_counts_as_no_branch() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(&root, app_entry().with_default_branch("  "), "", None, None);
    assert_eq!(git.git_ref, None);
    assert_eq!(git.ref_origin, None);
}

#[test]
fn an_explicit_workflow_ref_wins_over_the_repository_branch() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(
        &root,
        app_entry().with_default_branch("develop"),
        "    ref: release/1.0\n",
        None,
        None,
    );
    assert_eq!(git.git_ref.as_deref(), Some("release/1.0"));
    assert_eq!(git.ref_origin, Some(RefOrigin::Workflow));
}

#[test]
fn a_repository_chosen_by_the_caller_uses_its_branch_not_the_workflow_ref() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(
        &root,
        app_entry().with_default_branch("develop"),
        "    ref: release/1.0\n",
        None,
        Some("app"),
    );
    assert_eq!(git.git_ref.as_deref(), Some("develop"));
    assert_eq!(git.ref_origin, Some(RefOrigin::Repository));
}

#[test]
fn an_explicit_commit_wins_over_the_repository_branch() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(
        &root,
        app_entry().with_default_branch("develop"),
        "",
        Some(COMMIT),
        None,
    );
    assert_eq!(git.commit, COMMIT);
    assert_eq!(git.git_ref, None);
    assert_eq!(git.ref_origin, None);
}

#[test]
fn a_workflow_ref_that_is_a_commit_pins_it() {
    let root = TempRoot::new();
    let git = resolve_git_on_local(
        &root,
        app_entry().with_default_branch("develop"),
        &format!("    ref: {COMMIT}\n"),
        None,
        None,
    );
    assert_eq!(git.commit, COMMIT);
    assert_eq!(git.git_ref, None);
    assert_eq!(git.ref_origin, None);
}

#[test]
fn a_git_identity_saved_before_the_ref_origin_existed_still_reads() {
    let old = serde_json::json!({
        "repository": "app",
        "remote_url": "https://example.invalid/app.git",
        "commit": "",
        "git_ref": "develop",
        "credential": null
    });
    let git: narsor_engine::domain::GitIdentity = serde_json::from_value(old).unwrap();
    assert_eq!(git.git_ref.as_deref(), Some("develop"));
    assert_eq!(git.ref_origin, None);
    assert!(!serde_json::to_string(&git).unwrap().contains("ref_origin"));
}

#[test]
fn a_git_workspace_needs_a_full_pinned_commit() {
    let root = TempRoot::new();
    root.registry()
        .add(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_base_image(ImageSpec::pinned(pinned('d')))
                .with_git_credential("github-token"),
        )
        .unwrap();
    root.write_workflow("on-app", &repository_workflow("", ""));

    let missing = root.resolve(&intent("on-app")).unwrap_err();
    assert_eq!(missing.code(), "missing_commit");
    let short = root
        .resolve(&LaunchIntent {
            commit: Some("0123abc".to_string()),
            ..intent("on-app")
        })
        .unwrap_err();
    assert_eq!(short.code(), "invalid_commit");

    let launch = root.resolve(&on_app()).unwrap();
    let Some(TaskRepositorySource::Git(git)) = launch
        .spec
        .workspace
        .primary()
        .map(|repository| &repository.source)
    else {
        panic!("git workspace expected");
    };
    let credential = git.credential.as_ref().expect("credential reference");
    assert_eq!(credential.name, "github-token");
    assert_eq!(
        launch.spec.environment.secrets,
        vec![credential.clone()],
        "the clone credential is injected like every other secret"
    );

    let unknown = root
        .resolve(&LaunchIntent {
            repository: Some("nowhere".to_string()),
            ..on_app()
        })
        .unwrap_err();
    assert_eq!(unknown.code(), "unknown_repository");
}

// ---------------------------------------------------------------------------
// Environment identity (R18, R21)
// ---------------------------------------------------------------------------

#[test]
fn the_environment_identity_is_stable_across_processes() {
    // A literal, not a value computed in this process: the identity is
    // persisted and compared by other processes and other builds.
    assert_eq!(
        ResolvedEnvironment::harness_only("claude-code").identity,
        "sha256:4dd087d44c0f0bdfefe1411f24df47e2ca698f706235d0e83a49e4413360a1ef"
    );
}

#[test]
fn the_environment_identity_changes_with_every_pinned_component() {
    let root = TempRoot::new();
    let first = root.resolve(&intent("wf")).unwrap();
    let again = root.resolve(&intent("wf")).unwrap();
    assert_eq!(first, again, "resolution is deterministic");
    assert!(first.spec.digest.starts_with("sha256:"));
    assert!(first.spec.environment.identity.starts_with("sha256:"));
    assert!(first.spec.digest_is_valid());

    let base = first.spec.environment.clone();
    let mut variants: Vec<(&str, ResolvedEnvironment)> = Vec::new();
    let mut changed = base.clone();
    changed.base_image.as_mut().unwrap().digest = ImageDigest::parse(digest('1')).unwrap();
    variants.push(("base image", changed));
    let mut changed = base.clone();
    changed.runtime_bundle.as_mut().unwrap().digest = ImageDigest::parse(digest('2')).unwrap();
    variants.push(("runtime bundle", changed));
    let mut changed = base.clone();
    changed.profile.version += 1;
    variants.push(("profile version", changed));
    let mut changed = base.clone();
    changed.harness = "codex".to_string();
    variants.push(("harness", changed));
    let mut changed = base.clone();
    changed.tools.insert(
        "gh".to_string(),
        root.resolve(&intent("wf"))
            .unwrap()
            .spec
            .environment
            .base_image
            .unwrap(),
    );
    variants.push(("tool package", changed));
    for (component, variant) in variants {
        assert_ne!(
            variant.sealed().identity,
            base.identity,
            "changing the {component} must change the identity"
        );
    }

    // Through the catalogue, too: a profile edit that forgets to bump the
    // version still changes the identity, through the profile digest.
    root.catalog().save_profile(&profile(&[], &[])).unwrap();
    let edited = root.resolve(&intent("wf")).unwrap();
    assert_ne!(edited.spec.environment.identity, base.identity);
    let mut same_version = profile(&[], &[]);
    same_version.skills.push("another".to_string());
    root.catalog().save_profile(&same_version).unwrap();
    assert_ne!(
        root.resolve(&intent("wf"))
            .unwrap()
            .spec
            .environment
            .identity,
        edited.spec.environment.identity
    );
}

#[test]
fn two_tasks_for_one_repository_share_one_environment_identity() {
    // AE5: the same pinned image and bundle, not two builds of them.
    let root = TempRoot::new();
    let one = root
        .resolve(&LaunchIntent {
            inputs: serde_json::json!({ "ticket": "NAR-1" }),
            ..intent("wf")
        })
        .unwrap();
    let two = root
        .resolve(&LaunchIntent {
            inputs: serde_json::json!({ "ticket": "NAR-2" }),
            ..intent("wf")
        })
        .unwrap();
    assert_ne!(one.spec.digest, two.spec.digest);
    assert_eq!(one.spec.environment.identity, two.spec.environment.identity);
    assert_eq!(
        one.spec.environment.base_image.unwrap().pinned,
        workflow_image()
    );
}

// ---------------------------------------------------------------------------
// Several repositories (U3)
// ---------------------------------------------------------------------------

const MULTI_REPOSITORY: &str = "schema: v2\nname: multi\nversion: 1\ninputs:\n  - { name: docs, \
type: repository }\nglobals:\n  workspaces:\n    code: { primary: true, \
base_branch: main, repository: app }\n    docs: { base_branch: trunk, repository: \"{{ inputs.docs }}\" }\n\
states:\n  done:\n    description: done\nterminal_states: [done]\n";

fn multi_repository_root() -> TempRoot {
    let root = TempRoot::new();
    root.registry()
        .add(
            RepositoryEntry::new("app", "https://example.com/app.git")
                .with_default_branch("main")
                .with_base_image(ImageSpec::pinned(pinned('d'))),
        )
        .unwrap();
    root.registry()
        .add(
            RepositoryEntry::new("handbook", "https://example.com/handbook.git")
                .with_default_branch("trunk"),
        )
        .unwrap();
    root.write_workflow("multi", MULTI_REPOSITORY);
    root
}

fn multi_intent(inputs: serde_json::Value) -> LaunchIntent {
    LaunchIntent {
        inputs,
        commit: Some(COMMIT.to_string()),
        ..intent("multi")
    }
}

#[test]
fn a_task_records_every_repository_with_its_resolved_branch() {
    let root = multi_repository_root();
    let spec = root
        .resolve(&multi_intent(serde_json::json!({"docs": "handbook"})))
        .unwrap()
        .spec;

    let names: Vec<_> = spec
        .workspace
        .repositories
        .iter()
        .map(|repository| (repository.name.as_str(), repository.branch.as_deref()))
        .collect();
    // The fixed repository takes the workflow's branch, the one chosen by an
    // input takes the registry's default; the primary one goes first.
    assert_eq!(names, [("code", Some("main")), ("docs", Some("trunk"))]);
    let TaskRepositorySource::Git(docs) = &spec.workspace.repositories[1].source else {
        panic!("a git repository");
    };
    assert_eq!(docs.repository.as_deref(), Some("handbook"));
    assert!(
        !docs.is_pinned(),
        "only the primary takes the launch commit"
    );
    assert_eq!(docs.git_ref.as_deref(), Some("trunk"));
}

#[test]
fn the_digest_changes_with_a_repository_branch() {
    let root = multi_repository_root();
    let inputs = serde_json::json!({"docs": "handbook"});
    let spec = root.resolve(&multi_intent(inputs)).unwrap().spec;
    let mut moved = spec.clone();
    moved.workspace.repositories[1].branch = Some("other".to_string());
    assert_ne!(moved.compute_digest(), spec.digest);
}

#[test]
fn a_repository_chosen_by_an_unknown_input_is_refused() {
    let root = multi_repository_root();
    let error = root
        .resolve(&multi_intent(serde_json::json!({"docs": "nonesuch"})))
        .unwrap_err();
    assert_eq!(error.code(), "unknown_repository", "{error}");
}

// ---------------------------------------------------------------------------
// A branch chosen when the task is created
// ---------------------------------------------------------------------------

const CHOOSY: &str = "schema: v2\nname: choosy\nversion: 1\ninputs:\n  - { name: code, \
type: repository }\n  - { name: extra, type: repository }\nglobals:\n  workspaces:\n    \
code: { primary: true, base_branch: main, repository: \"{{ inputs.code }}\" }\n    \
docs: { base_branch: main, repository: handbook }\n    \
extra: { base_branch: lts, repository: \"{{ inputs.extra }}\" }\n\
states:\n  done:\n    description: done\nterminal_states: [done]\n";

fn choosy_root() -> TempRoot {
    let root = TempRoot::new();
    for entry in [
        RepositoryEntry::new("app", "https://example.com/app.git")
            .with_default_branch("main")
            .with_base_image(ImageSpec::pinned(pinned('d'))),
        RepositoryEntry::new("handbook", "https://example.com/handbook.git")
            .with_default_branch("trunk"),
        RepositoryEntry::new("lib", "https://example.com/lib.git"),
    ] {
        root.registry().add(entry).unwrap();
    }
    root.write_workflow("choosy", CHOOSY);
    root
}

fn choosy_intent(branches: &[(&str, &str)]) -> LaunchIntent {
    LaunchIntent {
        backend: Some("local".to_string()),
        inputs: serde_json::json!({"code": "app", "extra": "lib"}),
        branches: branches
            .iter()
            .map(|(input, branch)| (input.to_string(), branch.to_string()))
            .collect(),
        ..intent("choosy")
    }
}

/// The git identity and recorded branch of every repository, by name.
fn chosen(launch: &ResolvedLaunch) -> Vec<(String, Option<String>, Option<RefOrigin>)> {
    launch
        .spec
        .workspace
        .repositories
        .iter()
        .map(|repository| {
            let TaskRepositorySource::Git(git) = &repository.source else {
                panic!("a git repository");
            };
            assert_eq!(repository.branch, git.git_ref, "{}", repository.name);
            (repository.name.clone(), git.git_ref.clone(), git.ref_origin)
        })
        .collect()
}

#[test]
fn a_branch_chosen_for_an_input_wins_over_the_workflow_base_branch() {
    let root = choosy_root();
    let launch = root
        .resolve(&choosy_intent(&[("code", "develop")]))
        .unwrap();
    assert_eq!(
        chosen(&launch),
        [
            (
                "code".to_string(),
                Some("develop".to_string()),
                Some(RefOrigin::User)
            ),
            (
                "docs".to_string(),
                Some("main".to_string()),
                Some(RefOrigin::Workflow)
            ),
            (
                "extra".to_string(),
                Some("lts".to_string()),
                Some(RefOrigin::Workflow)
            ),
        ]
    );
}

#[test]
fn a_workspace_with_a_fixed_repository_keeps_its_base_branch() {
    let root = choosy_root();
    let launch = root.resolve(&choosy_intent(&[("extra", "next")])).unwrap();
    let docs = &chosen(&launch)[1];
    assert_eq!(
        docs,
        &(
            "docs".to_string(),
            Some("main".to_string()),
            Some(RefOrigin::Workflow)
        )
    );
}

#[test]
fn without_a_chosen_branch_the_workflow_base_branch_applies() {
    let root = choosy_root();
    let launch = root.resolve(&choosy_intent(&[])).unwrap();
    let names = chosen(&launch);
    assert_eq!(names[0].1.as_deref(), Some("main"));
    assert_eq!(names[2].1.as_deref(), Some("lts"));
    assert!(names
        .iter()
        .all(|(_, _, origin)| *origin == Some(RefOrigin::Workflow)));
}

/// A workflow of the first schema may name no branch at all: the repository's
/// `default_branch`, else the remote's `HEAD`, applies.
#[test]
fn a_chosen_branch_wins_over_the_repository_default_and_the_remote_head() {
    let root = TempRoot::new();
    root.registry()
        .add(app_entry().with_default_branch("develop"))
        .unwrap();
    root.registry()
        .add(RepositoryEntry::new(
            "bare",
            "https://example.invalid/bare.git",
        ))
        .unwrap();
    root.write_workflow(
        "pick",
        "name: pick\nversion: 1\ninputs:\n  - { name: repo, type: repository }\nglobals:\n  \
         workspace:\n    type: git\n    repository: \"{{ inputs.repo }}\"\nstates:\n  done:\n    \
         description: done\nterminal_states: [done]\n",
    );
    let launch = |repo: &str, branch: &str| {
        let branches = if branch.is_empty() {
            Default::default()
        } else {
            [("repo".to_string(), branch.to_string())].into()
        };
        let launch = root
            .resolve(&LaunchIntent {
                backend: Some("local".to_string()),
                inputs: serde_json::json!({ "repo": repo }),
                branches,
                ..intent("pick")
            })
            .unwrap();
        chosen(&launch).remove(0)
    };
    assert_eq!(launch("app", "release").1.as_deref(), Some("release"));
    assert_eq!(launch("app", "release").2, Some(RefOrigin::User));
    assert_eq!(launch("bare", "release").1.as_deref(), Some("release"));
    assert_eq!(launch("app", "").1.as_deref(), Some("develop"));
    assert_eq!(launch("bare", "").1, None);
}

#[test]
fn every_repository_input_takes_its_own_chosen_branch() {
    let root = choosy_root();
    let launch = root
        .resolve(&choosy_intent(&[("code", "develop"), ("extra", "next")]))
        .unwrap();
    let names = chosen(&launch);
    assert_eq!(names[0].1.as_deref(), Some("develop"));
    assert_eq!(names[2].1.as_deref(), Some("next"));
    assert_eq!(names[2].2, Some(RefOrigin::User));
}

#[test]
fn a_blank_chosen_branch_counts_as_no_choice_and_the_rest_is_trimmed() {
    let root = choosy_root();
    let launch = root
        .resolve(&choosy_intent(&[("code", "   "), ("extra", "  next ")]))
        .unwrap();
    let names = chosen(&launch);
    assert_eq!(
        names[0],
        (
            "code".to_string(),
            Some("main".to_string()),
            Some(RefOrigin::Workflow)
        )
    );
    assert_eq!(names[2].1.as_deref(), Some("next"));
}

#[test]
fn a_chosen_branch_that_looks_like_a_commit_stays_a_ref() {
    let root = choosy_root();
    let launch = root.resolve(&choosy_intent(&[("extra", COMMIT)])).unwrap();
    let TaskRepositorySource::Git(git) = &launch.spec.workspace.repositories[2].source else {
        panic!("a git repository");
    };
    assert!(!git.is_pinned());
    assert_eq!(git.git_ref.as_deref(), Some(COMMIT));
    assert_eq!(git.ref_origin, Some(RefOrigin::User));
}

#[test]
fn a_chosen_branch_for_no_repository_input_is_refused_naming_the_key() {
    let root = choosy_root();
    for key in ["nonesuch", "docs"] {
        let error = root
            .resolve(&choosy_intent(&[(key, "develop")]))
            .unwrap_err();
        assert_eq!(error.code(), "invalid_inputs", "{error}");
        assert!(error.to_string().contains(key), "{error}");
    }
}

#[test]
fn an_unusable_chosen_branch_is_refused_when_the_task_is_created() {
    let root = choosy_root();
    let too_long = "b".repeat(300);
    for branch in [
        "-x",
        "--upload-pack=evil",
        "a\nb",
        "a\u{1b}[31mb",
        &too_long,
    ] {
        let error = root
            .resolve(&choosy_intent(&[("code", branch)]))
            .expect_err(branch);
        assert_eq!(error.code(), "invalid_inputs", "{error}");
        let message = error.to_string();
        assert!(message.contains("'code'"), "{message}");
        assert!(
            !message.chars().any(char::is_control),
            "the message must be safe to print: {message:?}"
        );
    }
}

#[test]
fn an_image_backend_still_needs_a_commit_for_a_chosen_branch() {
    let root = choosy_root();
    let error = root
        .resolve(&LaunchIntent {
            backend: None,
            ..choosy_intent(&[("code", "develop")])
        })
        .unwrap_err();
    assert_eq!(error.code(), "missing_commit", "{error}");
}

#[test]
fn a_user_ref_origin_is_saved_as_user_and_read_back() {
    let json = serde_json::to_value(RefOrigin::User).unwrap();
    assert_eq!(json, serde_json::json!("user"));
    assert_eq!(
        serde_json::from_value::<RefOrigin>(json).unwrap(),
        RefOrigin::User
    );
    assert!(RefOrigin::User.describe().contains("użytkownik"));
}

#[test]
fn a_launch_spec_of_an_older_version_is_refused_naming_both_versions() {
    let root = TempRoot::new();
    let spec = root.resolve(&intent("wf")).unwrap().spec;
    let mut value = serde_json::to_value(&spec).unwrap();
    value["version"] = serde_json::json!(3);
    let error = serde_json::from_value::<TaskLaunchSpec>(value).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("version 3"), "{message}");
    assert!(
        message.contains(&format!("expected {TASK_LAUNCH_SPEC_VERSION}")),
        "{message}"
    );
}

// ---------------------------------------------------------------------------
// The launch spec record (KTD5)
// ---------------------------------------------------------------------------

#[test]
fn a_launch_spec_round_trips_and_detects_a_tampered_environment() {
    let root = TempRoot::new();
    let spec = root.resolve(&intent("wf")).unwrap().spec;
    assert_eq!(spec.version, TASK_LAUNCH_SPEC_VERSION);
    let text = serde_json::to_string(&spec).unwrap();
    let back: TaskLaunchSpec = serde_json::from_str(&text).unwrap();
    assert_eq!(back, spec);

    let mut tampered = spec.clone();
    tampered.environment.harness = "codex".to_string();
    assert!(!tampered.digest_is_valid());
    let mut resealed_digest_only = tampered.clone();
    resealed_digest_only.digest = resealed_digest_only.compute_digest();
    assert!(
        !resealed_digest_only.digest_is_valid(),
        "the environment identity is checked on its own as well"
    );
}

#[test]
fn a_launch_spec_carries_the_definition_it_was_resolved_with() {
    let root = TempRoot::new();
    let spec = root.resolve(&intent("wf")).unwrap().spec;
    let definition = spec.definition().expect("the snapshot reads back");
    assert_eq!(definition.id, "wf");
    assert_eq!(definition.version, spec.workflow_version);
    assert!(definition.states.contains_key(&definition.initial_state));

    // The snapshot is sealed with everything else: editing it breaks the
    // digest, and a snapshot of another workflow is refused on read.
    let mut tampered = spec.clone();
    tampered.definition.as_mut().unwrap()["name"] = serde_json::json!("other");
    assert!(!tampered.digest_is_valid());
    let mut other = definition.clone();
    other.id = "other".to_string();
    let mismatched = spec.clone().with_definition(&other).unwrap();
    assert!(mismatched.definition().is_err());
    let without = TaskLaunchSpec {
        definition: None,
        ..spec
    };
    assert!(without.definition().is_err(), "a runner never looks it up");
}

#[test]
fn a_snapshot_this_build_does_not_validate_is_refused_on_read() {
    let root = TempRoot::new();
    let spec = root.resolve(&intent("wf")).unwrap().spec;
    let mut foreign = spec.definition().unwrap();
    foreign.initial_state = "missing".to_string();
    let sealed_elsewhere = spec.with_definition(&foreign).unwrap();

    let error = sealed_elsewhere.definition().unwrap_err();

    assert!(
        matches!(error, LaunchDefinitionError::Invalid { .. }),
        "{error}"
    );
    assert!(error
        .to_string()
        .contains("Initial state 'missing' not found"));
}

#[test]
fn a_version_one_launch_spec_fails_clearly() {
    let v1 = serde_json::json!({
        "version": 1,
        "workflow_id": "wf",
        "workflow_version": 1,
        "inputs": {},
        "workspace": {},
        "digest": "fnv1a64-0000000000000000",
    });
    let error = serde_json::from_value::<TaskLaunchSpec>(v1).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("version 1"), "{message}");
    assert!(message.contains("create it again"), "{message}");
}

#[test]
fn a_backend_record_round_trips_through_its_file() {
    let root = TempRoot::new();
    let mut record = backend("k8s");
    record.description = Some("team cluster".to_string());
    record.limits.slot_release_grace_secs = 5;
    root.catalog().save_backend(&record).unwrap();
    assert_eq!(root.catalog().backend("k8s").unwrap(), Some(record));
    assert_eq!(root.catalog().backend("../k8s").unwrap(), None);
}

#[test]
fn slot_release_timings_are_read_fresh_from_the_backend_file() {
    use std::time::Duration;
    let root = TempRoot::new();
    let resolver = resolver(&root.path);
    let id = narsor_engine::domain::BackendId::parse("k8s").unwrap();

    let mut record = backend("k8s");
    root.catalog().save_backend(&record).unwrap();
    let defaults = resolver.slot_release_timings(&id).expect("defaults");
    assert_eq!(defaults.grace, Duration::from_secs(300));
    assert_eq!(defaults.read_extension, Duration::from_secs(180));
    assert_eq!(defaults.reader_idle, Duration::from_secs(120));

    record.limits.slot_release_grace_secs = 42;
    root.catalog().save_backend(&record).unwrap();
    let changed = resolver.slot_release_timings(&id).expect("changed");
    assert_eq!(changed.grace, Duration::from_secs(42));

    let yaml = std::fs::read_to_string(root.path().join("backends/k8s.yaml")).unwrap();
    std::fs::write(
        root.path().join("backends/k8s.yaml"),
        yaml.replace("slot_reader_idle_secs: 120", "slot_reader_idle_secs: 0"),
    )
    .unwrap();
    let error = resolver.slot_release_timings(&id).expect_err("refused");
    assert_eq!(error.code(), "invalid_backend");
}

// ---------------------------------------------------------------------------
// A workstation as a backend of a network server (U5: R13, R14)
// ---------------------------------------------------------------------------

/// Covers AE11.
#[test]
fn a_local_backend_under_another_id_must_be_a_workstation() {
    let root = TempRoot::new();
    let mut record = BackendRecord::new(
        "workstation",
        BackendKind::Local,
        TrustScope::Dedicated,
        backend("x").limits,
    );
    record.secrets = Vec::new();
    record.secret_delivery = Default::default();
    let problems = record.problems();
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("trust_scope")),
        "{problems:?}"
    );
    let error = root.catalog().save_backend(&record).expect_err("refused");
    assert!(error.to_string().contains("trust_scope"), "{error}");

    record.trust_scope = TrustScope::Workstation;
    assert!(record.problems().is_empty(), "{:?}", record.problems());
    // The daemon's own `local` keeps its scope rules as before.
    let builtin = BackendRecord::builtin_local();
    assert!(builtin.problems().is_empty());
}

#[test]
fn a_network_resolver_has_no_built_in_local_backend() {
    let root = TempRoot::new();
    root.write_workflow("bare", &workflow_yaml("bare", None));
    let network = resolver(root.path()).without_builtin_local();
    let local = narsor_engine::domain::BackendId::parse("local").unwrap();
    assert!(network.backend_record(&local).unwrap().is_none());
    assert!(resolver(root.path())
        .backend_record(&local)
        .unwrap()
        .is_some());

    let error = network
        .resolve(&LaunchIntent {
            backend: Some("local".to_string()),
            ..intent("bare")
        })
        .expect_err("refused at creation");
    assert_eq!(error.code(), "unknown_backend");
    let message = error.to_string();
    assert!(message.contains("network server"), "{message}");
    assert!(!message.contains("default_backend"), "{message}");
}

#[test]
fn a_network_resolver_defaulting_to_local_names_the_setting() {
    let root = TempRoot::new();
    root.write_workflow("bare", &workflow_yaml("bare", None));
    let network = narsor_engine::environment::CatalogLaunchResolver::new(
        root.path(),
        narsor_engine::workflows::WorkflowResolver::with_user_dir(root.path(), None),
        narsor_engine::environment::OwnerDefaults {
            default_backend: "local".to_string(),
            default_profile: "default".to_string(),
            local_workspace_roots: Vec::new(),
        },
        Arc::new(narsor_engine::environment::StaticHarnessDefaults::builtin(
            "claude-code",
        )),
    )
    .without_builtin_local();
    let error = network.resolve(&intent("bare")).expect_err("refused");
    assert!(error.to_string().contains("default_backend"), "{error}");
}

#[test]
fn a_declared_workstation_backend_resolves_on_a_network_server() {
    let root = TempRoot::new();
    root.write_workflow("bare", &workflow_yaml("bare", None));
    let record = BackendRecord::new(
        "laptop",
        BackendKind::Local,
        TrustScope::Workstation,
        backend("x").limits,
    );
    root.catalog().save_backend(&record).unwrap();
    let launch = resolver(root.path())
        .without_builtin_local()
        .resolve(&LaunchIntent {
            backend: Some("laptop".to_string()),
            ..intent("bare")
        })
        .expect("the laptop takes tasks");
    assert_eq!(launch.backend_id.as_str(), "laptop");
}
