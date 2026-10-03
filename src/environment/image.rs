//! Turning an image tag into a digest at task creation (R15, R18).
//!
//! A reference under [`DigestPolicy::Resolve`](crate::domain::DigestPolicy)
//! names a tag that must become a digest *before* the task exists, so the
//! task never depends on what a tag points to later. Talking to a registry is
//! a deployment concern (credentials, mirrors, air-gapped installs), so the
//! lookup sits behind [`ImageResolver`]; the control plane's default refuses
//! every tag, which keeps an unconfigured installation on pinned digests.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::RwLock;

use crate::domain::{ImageDigest, ImageReference};

/// Resolves `repository:tag` to the digest it points at right now.
pub trait ImageResolver: Send + Sync + fmt::Debug {
    /// The digest `reference`'s tag points at. `reference` carries a tag and
    /// no digest. The `Err` is a one-line reason for the owner.
    fn resolve_tag(&self, reference: &ImageReference) -> Result<ImageDigest, String>;
}

/// Refuses every tag: the default until a deployment wires a registry
/// lookup in.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTagResolution;

impl ImageResolver for NoTagResolution {
    fn resolve_tag(&self, _reference: &ImageReference) -> Result<ImageDigest, String> {
        Err(
            "this control plane resolves no image tags; pin the image as 'name@sha256:…'"
                .to_string(),
        )
    }
}

/// A fixed `repository:tag → digest` table — for tests, and for an
/// installation that mirrors a known set of images.
#[derive(Debug, Default)]
pub struct StaticImageResolver {
    tags: RwLock<BTreeMap<String, ImageDigest>>,
}

impl StaticImageResolver {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Points `repository:tag` at `digest` (replacing an earlier entry, which
    /// is how a test moves a tag).
    pub fn set(&self, repository_and_tag: impl Into<String>, digest: ImageDigest) {
        self.tags
            .write()
            .expect("image table lock poisoned")
            .insert(repository_and_tag.into(), digest);
    }

    /// Builder form of [`Self::set`].
    pub fn with(self, repository_and_tag: impl Into<String>, digest: ImageDigest) -> Self {
        self.set(repository_and_tag, digest);
        self
    }
}

impl ImageResolver for StaticImageResolver {
    fn resolve_tag(&self, reference: &ImageReference) -> Result<ImageDigest, String> {
        let key = reference.to_string();
        self.tags
            .read()
            .expect("image table lock poisoned")
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("tag '{key}' is not known to the image resolver"))
    }
}
