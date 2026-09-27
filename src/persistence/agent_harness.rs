//! Durable, live selection of the daemon's default agent harness.
//!
//! The configured value remains the fallback.  This module stores only an
//! optional operator override in a small versioned envelope under `state_dir`.
//! A mutation holds one async mutex across validation, the blocking atomic
//! write, and snapshot publication, so callers can never observe a value that
//! was acknowledged but did not reach disk.

use super::{read_json, write_json_atomic, PersistenceError, StatePaths};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::Mutex;

const ENVELOPE_VERSION: u32 = 1;

/// The authoritative view returned to daemon consumers and, later, the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSelectionSnapshot {
    /// Names registered by this running version, in registry order.
    pub supported: Vec<String>,
    /// The value supplied by startup configuration.
    pub configured: String,
    /// The durable operator choice, if one is active.
    pub override_harness: Option<String>,
    /// The active value: `override_harness` when present, otherwise
    /// `configured`.
    pub effective: String,
    /// Monotonically increasing durable mutation number.
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSelection {
    version: u32,
    revision: u64,
    #[serde(rename = "override")]
    override_harness: Option<String>,
}

#[derive(Debug)]
struct SelectionState {
    snapshot: HarnessSelectionSnapshot,
}

/// Startup or mutation failure for a default-harness selection.
#[derive(Debug, Error)]
pub enum HarnessSelectionError {
    /// The state file could not be read or atomically replaced.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),

    /// A configured or persisted value is not offered by the harness registry.
    #[error("unsupported harness '{harness}' in {origin}; supported harnesses are: {supported}")]
    UnsupportedHarness {
        origin: &'static str,
        harness: String,
        supported: String,
    },

    /// The on-disk envelope belongs to a version this binary cannot interpret.
    #[error(
        "unsupported agent harness selection version {found} in '{path}' (expected {expected})"
    )]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },

    /// The registry passed an invalid closed set to the selector.
    #[error("invalid supported harness registry: {message}")]
    InvalidRegistry { message: String },

    /// The blocking filesystem worker unexpectedly stopped.
    #[error("agent harness selection filesystem task failed: {0}")]
    BlockingTask(#[from] tokio::task::JoinError),
}

/// One shared, concurrency-safe default-harness selector.
#[derive(Debug)]
pub struct DefaultHarnessSelector {
    path: PathBuf,
    supported: Vec<String>,
    configured: String,
    state: Mutex<SelectionState>,
}

impl DefaultHarnessSelector {
    /// Loads the persisted override, if any, and validates it against the
    /// supplied harness registry. A missing file is the backward-compatible
    /// initial state and deliberately does not create a file.
    pub async fn load(
        paths: StatePaths,
        supported: Vec<String>,
        configured: impl Into<String>,
    ) -> Result<Self, HarnessSelectionError> {
        validate_registry(&supported)?;
        let configured = configured.into();
        validate_harness("configured value", &configured, &supported)?;
        let path = paths.agent_harness_selection_file();
        let read_path = path.clone();
        let persisted = tokio::task::spawn_blocking(move || read_persisted(&read_path)).await??;

        let (override_harness, revision) = match persisted {
            Some(envelope) => {
                if envelope.version != ENVELOPE_VERSION {
                    return Err(HarnessSelectionError::UnsupportedVersion {
                        path,
                        found: envelope.version,
                        expected: ENVELOPE_VERSION,
                    });
                }
                if let Some(harness) = &envelope.override_harness {
                    validate_harness("persisted override", harness, &supported)?;
                }
                (envelope.override_harness, envelope.revision)
            }
            None => (None, 0),
        };
        let snapshot = snapshot(&supported, &configured, override_harness, revision);

        Ok(Self {
            path,
            supported,
            configured,
            state: Mutex::new(SelectionState { snapshot }),
        })
    }

    /// Returns an immutable point-in-time selection view.
    pub async fn snapshot(&self) -> HarnessSelectionSnapshot {
        self.state.lock().await.snapshot.clone()
    }

    /// Durably changes the default. Repeating the active override is a no-op.
    pub async fn set(
        &self,
        harness: impl AsRef<str>,
    ) -> Result<HarnessSelectionSnapshot, HarnessSelectionError> {
        let harness = harness.as_ref();
        validate_harness("requested value", harness, &self.supported)?;
        self.mutate(Some(harness.to_owned())).await
    }

    /// Durably removes the override and immediately exposes the configured
    /// fallback. Reset writes `override: null`; it never deletes the file.
    pub async fn reset(&self) -> Result<HarnessSelectionSnapshot, HarnessSelectionError> {
        self.mutate(None).await
    }

    async fn mutate(
        &self,
        override_harness: Option<String>,
    ) -> Result<HarnessSelectionSnapshot, HarnessSelectionError> {
        // This deliberately covers blocking I/O too.  It is a short critical
        // section and gives every successful caller an unambiguous order.
        let mut state = self.state.lock().await;
        if state.snapshot.override_harness == override_harness {
            return Ok(state.snapshot.clone());
        }

        let revision = state
            .snapshot
            .revision
            .checked_add(1)
            .expect("agent harness selection revision overflowed");
        let envelope = PersistedSelection {
            version: ENVELOPE_VERSION,
            revision,
            override_harness: override_harness.clone(),
        };
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_json_atomic(&path, &envelope)).await??;

        // Publish only after the atomic replacement succeeds.
        let next = snapshot(
            &self.supported,
            &self.configured,
            override_harness,
            revision,
        );
        state.snapshot = next.clone();
        Ok(next)
    }
}

fn read_persisted(
    path: &std::path::Path,
) -> Result<Option<PersistedSelection>, HarnessSelectionError> {
    match read_json(path) {
        Ok(value) => Ok(Some(value)),
        Err(PersistenceError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_registry(supported: &[String]) -> Result<(), HarnessSelectionError> {
    if supported.is_empty() {
        return Err(HarnessSelectionError::InvalidRegistry {
            message: "no harnesses were registered".into(),
        });
    }
    if supported.iter().any(|name| name.is_empty()) {
        return Err(HarnessSelectionError::InvalidRegistry {
            message: "a registered harness name is empty".into(),
        });
    }
    let unique: HashSet<_> = supported.iter().collect();
    if unique.len() != supported.len() {
        return Err(HarnessSelectionError::InvalidRegistry {
            message: "a harness name was registered more than once".into(),
        });
    }
    Ok(())
}

fn validate_harness(
    origin: &'static str,
    harness: &str,
    supported: &[String],
) -> Result<(), HarnessSelectionError> {
    if supported.iter().any(|known| known == harness) {
        return Ok(());
    }
    Err(HarnessSelectionError::UnsupportedHarness {
        origin,
        harness: harness.to_owned(),
        supported: supported.join(", "),
    })
}

fn snapshot(
    supported: &[String],
    configured: &str,
    override_harness: Option<String>,
    revision: u64,
) -> HarnessSelectionSnapshot {
    HarnessSelectionSnapshot {
        supported: supported.to_vec(),
        configured: configured.to_owned(),
        effective: override_harness.as_deref().unwrap_or(configured).to_owned(),
        override_harness,
        revision,
    }
}
