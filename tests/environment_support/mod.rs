//! A minimal, valid environment catalogue for tests that create remote
//! tasks through the control plane (U6).
//!
//! Since U6 every create request is resolved against configuration before
//! anything is stored, so a test that only cares about orchestration still
//! needs a backend, a runtime bundle and a workflow with an image. This
//! module writes exactly that under a catalogue root and builds the
//! resolver the control plane uses.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use narsor::domain::{ImageSpec, ResourceProfile, SecretDelivery};
use narsor::environment::{
    BackendKind, BackendLimits, BackendRecord, BackendSecret, CatalogLaunchResolver,
    EnvironmentCatalog, HarnessDefaults, LaunchResolver, OwnerDefaults, RuntimeCatalog,
    SecretDeliveryMode, StaticHarnessDefaults, TrustScope,
};
use narsor::workflows::WorkflowResolver;

/// Backends seeded by [`seed_catalog`].
pub const BACKENDS: [&str; 4] = ["k8s", "backend-a", "backend-b", "other-backend"];

/// Workflows seeded by [`seed_catalog`], all at version 1.
pub const WORKFLOWS: [&str; 2] = ["implement-feature", "wf"];

/// A digest made of one repeated hex character.
pub fn digest(ch: char) -> String {
    format!("sha256:{}", ch.to_string().repeat(64))
}

/// The base image every seeded workflow pins in `globals.image`.
pub fn workflow_image() -> String {
    format!("ghcr.io/acme/dev@{}", digest('a'))
}

/// A Kubernetes backend record with a GitHub token on its allow list.
pub fn backend(id: &str) -> BackendRecord {
    let mut record = BackendRecord::new(
        id,
        BackendKind::Kubernetes,
        TrustScope::Dedicated,
        BackendLimits {
            max_concurrent_tasks: 3,
            default_resources: ResourceProfile::new(1000, 2048),
            max_resources: Some(ResourceProfile::new(4000, 8192)),
        },
    );
    record.secret_delivery = SecretDeliveryMode::Platform;
    record.secrets = vec![BackendSecret {
        name: "github-token".to_string(),
        tool: "gh".to_string(),
        key: "narsor-github/token".to_string(),
        delivery: SecretDelivery::Env("GH_TOKEN".to_string()),
    }];
    record
}

/// A workflow with one script step and a pinned image.
pub fn workflow_yaml(name: &str, image: Option<&str>) -> String {
    let globals = image
        .map(|image| format!("globals:\n  image: \"{image}\"\n"))
        .unwrap_or_default();
    format!(
        "name: {name}\nversion: 1\n{globals}states:\n  work:\n    action:\n      type: script\n      \
         command: \"true\"\n    outputs: [success]\n    transitions:\n      success: done\n  \
         done:\n    description: finished\nterminal_states: [done]\n"
    )
}

/// Writes backends, the runtime catalogue and the workflows under `root`.
pub fn seed_catalog(root: &Path) {
    let catalog = EnvironmentCatalog::new(root);
    for id in BACKENDS {
        catalog.save_backend(&backend(id)).expect("save backend");
    }
    catalog
        .save_runtime(&RuntimeCatalog {
            bundle: ImageSpec::pinned(format!("ghcr.io/narsor/runtime@{}", digest('b'))),
            tools: [(
                "gh".to_string(),
                ImageSpec::pinned(format!("ghcr.io/narsor/tools/gh@{}", digest('c'))),
            )]
            .into_iter()
            .collect(),
        })
        .expect("save runtime");
    let workflows = root.join("workflows");
    std::fs::create_dir_all(&workflows).expect("workflows dir");
    for name in WORKFLOWS {
        std::fs::write(
            workflows.join(format!("{name}.yaml")),
            workflow_yaml(name, Some(&workflow_image())),
        )
        .expect("write workflow");
    }
}

/// The catalogue resolver over `root`, defaulting to backend `k8s`.
pub fn resolver(root: &Path) -> CatalogLaunchResolver {
    resolver_with_local_roots(root, vec![root.to_path_buf()])
}

/// Like [`resolver`], with an explicit allow-list of host directories a
/// local workspace may be taken from.
pub fn resolver_with_local_roots(
    root: &Path,
    local_workspace_roots: Vec<std::path::PathBuf>,
) -> CatalogLaunchResolver {
    resolver_with(
        root,
        local_workspace_roots,
        Arc::new(StaticHarnessDefaults::builtin("claude-code")),
    )
}

/// Like [`resolver`], with the harness defaults of `harness` — a live
/// selector makes the control plane hand running tasks its default (U7).
pub fn resolver_with(
    root: &Path,
    local_workspace_roots: Vec<std::path::PathBuf>,
    harness: Arc<dyn HarnessDefaults>,
) -> CatalogLaunchResolver {
    CatalogLaunchResolver::new(
        root,
        WorkflowResolver::with_user_dir(root, None),
        OwnerDefaults {
            default_backend: "k8s".to_string(),
            default_profile: "default".to_string(),
            local_workspace_roots,
        },
        harness,
    )
}

/// Seeds `root` and returns its resolver, ready for `ProtocolService::new`.
pub fn seeded_catalog_resolver(root: &Path) -> Arc<CatalogLaunchResolver> {
    seed_catalog(root);
    Arc::new(resolver(root))
}

/// Seeds `root` and returns its resolver over the harness defaults of
/// `harness`.
pub fn seeded_catalog_resolver_with(
    root: &Path,
    harness: Arc<dyn HarnessDefaults>,
) -> Arc<CatalogLaunchResolver> {
    seed_catalog(root);
    Arc::new(resolver_with(root, vec![root.to_path_buf()], harness))
}

/// Lets backend `id` run only `harnesses` from now on.
pub fn restrict_harnesses(root: &Path, id: &str, harnesses: &[&str]) {
    let mut record = backend(id);
    record.harnesses = harnesses.iter().map(|name| name.to_string()).collect();
    EnvironmentCatalog::new(root)
        .save_backend(&record)
        .expect("save backend");
}

pub fn seeded_resolver(root: &Path) -> Arc<dyn LaunchResolver> {
    seeded_catalog_resolver(root)
}
