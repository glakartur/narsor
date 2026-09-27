//! Durable default-harness selection, exercised against the real filesystem.

use narsor::persistence::{DefaultHarnessSelector, StatePaths};
use std::fs;
use std::path::{Path, PathBuf};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "narsor-agent-harness-selection-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn supported() -> Vec<String> {
    vec!["claude-code".into(), "codex".into(), "opencode".into()]
}

#[tokio::test]
async fn missing_file_uses_configured_harness_without_an_override() {
    let root = TempRoot::new();
    let selector =
        DefaultHarnessSelector::load(StatePaths::new(root.path()), supported(), "claude-code")
            .await
            .expect("missing selection file is initial state");

    assert_eq!(
        selector.snapshot().await,
        narsor::persistence::HarnessSelectionSnapshot {
            supported: supported(),
            configured: "claude-code".into(),
            override_harness: None,
            effective: "claude-code".into(),
            revision: 0,
        }
    );
}

#[tokio::test]
async fn override_survives_reload_and_reset_persists_null() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    let selector = DefaultHarnessSelector::load(paths.clone(), supported(), "claude-code")
        .await
        .unwrap();

    let set = selector.set("codex").await.unwrap();
    assert_eq!(set.override_harness.as_deref(), Some("codex"));
    assert_eq!(set.effective, "codex");
    assert_eq!(set.revision, 1);

    let reloaded = DefaultHarnessSelector::load(paths.clone(), supported(), "opencode")
        .await
        .unwrap();
    assert_eq!(reloaded.snapshot().await.effective, "codex");

    let reset = reloaded.reset().await.unwrap();
    assert_eq!(reset.override_harness, None);
    assert_eq!(reset.effective, "opencode");
    assert_eq!(reset.revision, 2);
    let json: serde_json::Value =
        serde_json::from_slice(&fs::read(paths.agent_harness_selection_file()).unwrap()).unwrap();
    assert_eq!(json["override"], serde_json::Value::Null);
}

#[tokio::test]
async fn invalid_or_corrupt_persisted_state_is_rejected_at_startup() {
    let root = TempRoot::new();
    let paths = StatePaths::new(root.path());
    fs::write(
        paths.agent_harness_selection_file(),
        r#"{"version":1,"revision":4,"override":"unknown"}"#,
    )
    .unwrap();
    let error = DefaultHarnessSelector::load(paths.clone(), supported(), "claude-code")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsupported harness 'unknown'"));

    fs::write(paths.agent_harness_selection_file(), b"not json").unwrap();
    let error = DefaultHarnessSelector::load(paths, supported(), "claude-code")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failed to deserialize"));
}

#[tokio::test]
async fn failed_persistence_never_publishes_a_new_snapshot() {
    let root = TempRoot::new();
    let state_root = root.path().join("state");
    fs::create_dir(&state_root).unwrap();
    let selector =
        DefaultHarnessSelector::load(StatePaths::new(&state_root), supported(), "claude-code")
            .await
            .unwrap();
    fs::remove_dir(&state_root).unwrap();
    fs::write(&state_root, b"file").unwrap();

    assert!(selector.set("codex").await.is_err());
    let snapshot = selector.snapshot().await;
    assert_eq!(snapshot.effective, "claude-code");
    assert_eq!(snapshot.override_harness, None);
    assert_eq!(snapshot.revision, 0);
}

#[tokio::test]
async fn mutations_are_serialized_and_idempotent() {
    let root = TempRoot::new();
    let selector = std::sync::Arc::new(
        DefaultHarnessSelector::load(StatePaths::new(root.path()), supported(), "claude-code")
            .await
            .unwrap(),
    );
    let first = {
        let selector = selector.clone();
        tokio::spawn(async move { selector.set("codex").await.unwrap() })
    };
    let second = {
        let selector = selector.clone();
        tokio::spawn(async move { selector.reset().await.unwrap() })
    };
    let a = first.await.unwrap();
    let b = second.await.unwrap();
    assert_ne!(a.revision, b.revision);
    assert_eq!(selector.snapshot().await.revision, 2);

    let stable = selector.snapshot().await;
    assert_eq!(selector.reset().await.unwrap(), stable);
}
