//! User profiles (R16) and the runtime catalogue (R20).
//!
//! A profile is what one user adds to a repository's environment: allowed
//! settings, skills, tool packages and the names of the credentials they
//! need. It is stored as `{catalog}/profiles/{id}.yaml`:
//!
//! ```yaml
//! version: 3
//! harness: claude-code          # optional preferred harness
//! settings: { editor: vim }
//! skills: [code-review]
//! tools: [gh]
//! secrets: [github-token]       # names from the backend's allow list
//! ```
//!
//! The `version` is the owner's own counter (R18); the resolved environment
//! additionally records a digest of the whole profile, so an edit that forgot
//! to bump it still yields a different environment identity.
//!
//! The runtime catalogue, `{catalog}/runtime.yaml`, names the Narsor runtime
//! bundle and the tool packages profiles may pick from:
//!
//! ```yaml
//! bundle: ghcr.io/narsor/runtime@sha256:…
//! tools:
//!   gh: { reference: ghcr.io/narsor/tools/gh:2.60, digest_policy: resolve }
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::{canonical_sha256, ImageSpec, ResolvedProfile};

use super::backend::is_reference_name;

/// The profile used when nothing names one and no file defines it.
pub const DEFAULT_PROFILE: &str = "default";

/// One user profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRecord {
    /// Identifier, carried by the file name.
    #[serde(skip)]
    pub id: String,
    /// The owner's version counter.
    pub version: u32,
    /// Free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Preferred harness for agent steps that name none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Allowed settings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    /// Skills, materialized from the runtime bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Tool packages from the runtime catalogue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Names of backend secret references the tools need.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl ProfileRecord {
    /// An empty profile at version 0 — what [`DEFAULT_PROFILE`] means when no
    /// file defines it, so a fresh installation needs no profile file.
    pub fn empty(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// Every problem with the profile, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if !is_reference_name(&self.id) {
            problems.push(format!("profile id '{}' is not a usable name", self.id));
        }
        if let Some(harness) = &self.harness {
            if harness.trim().is_empty() {
                problems.push("harness is blank".to_string());
            }
        }
        for key in self.settings.keys() {
            if !is_reference_name(key) {
                problems.push(format!("settings: key '{key}' is not a usable name"));
            }
        }
        for (field, names) in [
            ("skills", &self.skills),
            ("tools", &self.tools),
            ("secrets", &self.secrets),
        ] {
            let mut seen = BTreeSet::new();
            for name in names {
                if !is_reference_name(name) {
                    problems.push(format!("{field}: '{name}' is not a usable name"));
                } else if !seen.insert(name) {
                    problems.push(format!("{field}: '{name}' is listed twice"));
                }
            }
        }
        problems
    }

    /// The profile as a task records it.
    pub fn resolve(&self) -> ResolvedProfile {
        let mut skills = self.skills.clone();
        skills.sort();
        let mut tools = self.tools.clone();
        tools.sort();
        let content = serde_json::to_value(self).expect("a profile always serializes");
        ResolvedProfile {
            id: self.id.clone(),
            version: self.version,
            digest: canonical_sha256(&content),
            settings: self.settings.clone(),
            skills,
            tools,
        }
    }
}

/// The runtime bundle and the tool packages (R20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCatalog {
    /// The Narsor runtime bundle OCI image.
    pub bundle: ImageSpec,
    /// Tool package images, by tool name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ImageSpec>,
}

impl RuntimeCatalog {
    /// Every problem with the catalogue, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(error) = self.bundle.check() {
            problems.push(format!("bundle: {error}"));
        }
        for (name, image) in &self.tools {
            if !is_reference_name(name) {
                problems.push(format!("tools: '{name}' is not a usable name"));
            }
            if let Err(error) = image.check() {
                problems.push(format!("tools.{name}: {error}"));
            }
        }
        problems
    }
}
