//! The pinned environment a remote task runs in (R15-R22, R39, R42; KTD5).
//!
//! Everything here is shared by both sides of KTD1's boundary: the control
//! plane resolves these values once, when a task is created, and the runner
//! (U3) and the environment controller (U5) read the very same values from
//! the task's [`TaskLaunchSpec`](super::TaskLaunchSpec). The configuration
//! that *produces* them — backends, profiles, the runtime catalogue — is
//! control-plane only and lives in [`crate::environment`].
//!
//! # Nothing secret, by construction
//!
//! A [`SecretReference`] names where a credential lives on the backend and
//! how the controller hands it to the tool; it never carries the value. The
//! one type that does carry a value, [`SecretValue`], deliberately has no
//! `Serialize` and a redacting `Debug`, so it cannot end up in a record, an
//! API response, an event or a log line by accident (R19, KTD4).
//!
//! # Identities are SHA-256
//!
//! [`ResolvedEnvironment::identity`] is a hash of every pinned component
//! (R21). It is persisted, compared across processes and — unlike the
//! accident checks elsewhere — is meant to resist a deliberate collision: a
//! controller's admission policy may key image and secret approval on it.
//! The hash runs over a canonical JSON rendering whose object keys are
//! sorted here rather than by whatever map type `serde_json` was built
//! with, so the value cannot drift with a dependency's feature flags.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::BackendId;

/// Prefix of every SHA-256 digest this crate writes.
pub const SHA256_PREFIX: &str = "sha256:";

/// Hex characters in a SHA-256 digest.
const SHA256_HEX_LEN: usize = 64;

/// Longest image reference accepted (the OCI distribution limit on a
/// repository name is 255; a tag and a digest add at most ~200 more).
const MAX_IMAGE_REFERENCE_LEN: usize = 512;

/// Longest tag the OCI distribution spec allows.
const MAX_TAG_LEN: usize = 128;

// ---------------------------------------------------------------------------
// Canonical hashing
// ---------------------------------------------------------------------------

/// `sha256:<hex>` of `value`'s canonical JSON rendering.
///
/// Canonical means: object keys sorted bytewise at every depth, no
/// whitespace. The serializer of `serde_json` is used only for scalars, so
/// the result does not depend on `serde_json`'s `preserve_order` feature.
pub fn canonical_sha256(value: &serde_json::Value) -> String {
    let mut bytes = Vec::new();
    write_canonical(value, &mut bytes);
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(SHA256_PREFIX.len() + SHA256_HEX_LEN);
    out.push_str(SHA256_PREFIX);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut Vec<u8>) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push(b'{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).expect("a string always serializes"));
                out.push(b':');
                write_canonical(item, out);
            }
            out.push(b'}');
        }
        serde_json::Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        scalar => out.extend(serde_json::to_vec(scalar).expect("a scalar always serializes")),
    }
}

fn is_lower_hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Why an image reference or digest was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImageError {
    /// The text is not an OCI image reference this crate accepts.
    #[error("image reference '{reference}' is invalid: {reason}")]
    InvalidReference {
        /// The reference as written.
        reference: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The digest is not `sha256:` and 64 lowercase hex characters.
    #[error(
        "image digest '{0}' is invalid: expected 'sha256:' followed by 64 lowercase hex characters"
    )]
    InvalidDigest(String),
    /// The policy requires a digest and the reference carries only a tag.
    #[error(
        "image reference '{0}' has no digest; pin it as 'name@sha256:…' or set \
         'digest_policy: resolve' to resolve its tag before task creation"
    )]
    Unpinned(String),
}

/// A content digest of an OCI image (`sha256:<64 hex>`).
///
/// Only SHA-256 is accepted: it is what every registry serves today, and a
/// second algorithm would be a second thing an admission policy has to know.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageDigest(String);

impl ImageDigest {
    /// Validates and wraps `value`.
    pub fn parse(value: impl Into<String>) -> Result<Self, ImageError> {
        let value = value.into();
        let valid = value
            .strip_prefix(SHA256_PREFIX)
            .is_some_and(|hex| hex.len() == SHA256_HEX_LEN && is_lower_hex(hex));
        if valid {
            Ok(Self(value))
        } else {
            Err(ImageError::InvalidDigest(value))
        }
    }

    /// The digest as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ImageDigest {
    type Error = ImageError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ImageDigest> for String {
    fn from(value: ImageDigest) -> Self {
        value.0
    }
}

impl fmt::Display for ImageDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A parsed OCI image reference: `repository[:tag][@sha256:…]`.
///
/// Stricter than what container runtimes accept, on purpose: an implicit
/// `latest` is refused (a tag or a digest must be written), so a reference
/// never silently means "whatever was pushed last"; and a leading `-` can
/// never appear, so a reference cannot be read as a command-line option by a
/// tool it is later handed to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImageReference {
    /// Registry host (optionally with port) and path, e.g.
    /// `ghcr.io/acme/api-dev`.
    pub repository: String,
    /// The tag, when one was written.
    pub tag: Option<String>,
    /// The digest, when one was written.
    pub digest: Option<ImageDigest>,
}

impl ImageReference {
    /// Parses one reference.
    pub fn parse(reference: &str) -> Result<Self, ImageError> {
        let invalid = |reason: &str| ImageError::InvalidReference {
            reference: reference.to_string(),
            reason: reason.to_string(),
        };
        if reference.is_empty() {
            return Err(invalid("it is empty"));
        }
        if reference.len() > MAX_IMAGE_REFERENCE_LEN {
            return Err(invalid("it is too long"));
        }
        if reference
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(invalid("it contains whitespace"));
        }

        let (name, digest) = match reference.split_once('@') {
            Some((name, digest)) => (name, Some(ImageDigest::parse(digest)?)),
            None => (reference, None),
        };
        let last_slash = name.rfind('/').map_or(0, |index| index + 1);
        let (repository, tag) = match name[last_slash..].rfind(':') {
            Some(colon) => {
                let colon = last_slash + colon;
                (&name[..colon], Some(&name[colon + 1..]))
            }
            None => (name, None),
        };

        validate_repository(repository).map_err(|reason| invalid(&reason))?;
        if let Some(tag) = tag {
            validate_tag(tag).map_err(|reason| invalid(&reason))?;
        }
        if tag.is_none() && digest.is_none() {
            return Err(invalid(
                "write a tag or a digest explicitly; an implicit 'latest' is not accepted",
            ));
        }
        Ok(Self {
            repository: repository.to_string(),
            tag: tag.map(str::to_string),
            digest,
        })
    }

    /// `repository@digest` — what a backend pulls, independent of any tag.
    pub fn pinned(&self, digest: &ImageDigest) -> String {
        format!("{}@{digest}", self.repository)
    }
}

impl fmt::Display for ImageReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.is_empty() {
        return Err("the repository name is empty".to_string());
    }
    for (index, component) in repository.split('/').enumerate() {
        // Only the first component may be a registry host with a port.
        let component = if index == 0 {
            match component.split_once(':') {
                Some((host, port)) => {
                    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(format!("'{component}' has an invalid port"));
                    }
                    host
                }
                None => component,
            }
        } else {
            component
        };
        let valid = !component.is_empty()
            && component
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric())
            && component.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        if !valid {
            return Err(format!(
                "path component '{component}' must be lowercase letters, digits, '.', '_' \
                 or '-', starting with a letter or digit"
            ));
        }
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), String> {
    let valid = !tag.is_empty()
        && tag.len() <= MAX_TAG_LEN
        && tag
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric() || first == b'_')
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "tag '{tag}' must be 1-{MAX_TAG_LEN} characters of [A-Za-z0-9_.-], not starting \
             with '.' or '-'"
        ))
    }
}

/// How a configured image reference becomes a digest (R15, R18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestPolicy {
    /// The reference must already carry `@sha256:…`; a tag alone is a
    /// validation error. The default, because it needs no registry access
    /// and cannot change underneath a task.
    #[default]
    Pinned,
    /// A tag is resolved to a digest once, at task creation, through the
    /// control plane's image resolver; the task then keeps that digest.
    Resolve,
}

/// An image as configuration writes it: a reference and its digest policy.
///
/// Written either as a bare string (policy [`DigestPolicy::Pinned`]) or as a
/// mapping:
///
/// ```yaml
/// base_image: ghcr.io/acme/api-dev@sha256:…        # pinned
/// base_image:
///   reference: ghcr.io/acme/api-dev:2026.09
///   digest_policy: resolve
/// ```
///
/// The reference stays text here; [`ImageSpec::check`] parses it, so a
/// workflow validator can report a bad reference as a validation error
/// rather than as a file that does not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSpec {
    /// The reference as written.
    pub reference: String,
    /// How it becomes a digest.
    pub digest_policy: DigestPolicy,
}

impl ImageSpec {
    /// A spec that must already be pinned.
    pub fn pinned(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            digest_policy: DigestPolicy::Pinned,
        }
    }

    /// A spec whose tag is resolved at task creation.
    pub fn resolve(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            digest_policy: DigestPolicy::Resolve,
        }
    }

    /// Parses the reference and checks it against the policy.
    pub fn check(&self) -> Result<ImageReference, ImageError> {
        let parsed = ImageReference::parse(&self.reference)?;
        if self.digest_policy == DigestPolicy::Pinned && parsed.digest.is_none() {
            return Err(ImageError::Unpinned(self.reference.clone()));
        }
        Ok(parsed)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSpecMapping {
    reference: String,
    #[serde(default)]
    digest_policy: DigestPolicy,
}

impl Serialize for ImageSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.digest_policy {
            DigestPolicy::Pinned => serializer.serialize_str(&self.reference),
            DigestPolicy::Resolve => ImageSpecMapping {
                reference: self.reference.clone(),
                digest_policy: self.digest_policy,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ImageSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Reference(String),
            Mapping(ImageSpecMapping),
        }
        match Either::deserialize(deserializer).map_err(|_| {
            serde::de::Error::custom(
                "an image is a reference string, or a mapping with 'reference' and an \
                 optional 'digest_policy' (pinned | resolve)",
            )
        })? {
            Either::Reference(reference) => Ok(Self::pinned(reference)),
            Either::Mapping(mapping) => Ok(Self {
                reference: mapping.reference,
                digest_policy: mapping.digest_policy,
            }),
        }
    }
}

/// Where a resolved image was chosen from (R18).
///
/// For the base image this is the precedence outcome — a step override wins
/// over a workflow override, which wins over the repository (R15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// The registered repository's `base_image`.
    Repository {
        /// Repository id.
        repository: String,
    },
    /// The workflow's `globals.image`.
    Workflow {
        /// Workflow id.
        workflow: String,
    },
    /// One state's `image`.
    Step {
        /// Workflow id.
        workflow: String,
        /// State id.
        state: String,
    },
    /// The Narsor runtime bundle (R20).
    RuntimeBundle,
    /// A tool package of the runtime catalogue.
    Tool {
        /// Tool name.
        tool: String,
    },
}

/// An image pinned to a digest, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedImage {
    /// The reference exactly as configured (tag kept for humans).
    pub reference: String,
    /// The digest the task uses — never re-resolved for this task.
    pub digest: ImageDigest,
    /// `repository@digest`: what a backend pulls.
    pub pinned: String,
    /// True when the digest came from resolving a tag at creation.
    pub tag_resolved: bool,
    /// Where it was chosen from.
    pub source: ImageSource,
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// CPU and memory one task workload may use.
///
/// Whole millicores and mebibytes rather than Kubernetes quantity strings:
/// comparison against a backend's limits is then plain integer arithmetic,
/// and U5 renders `{cpu}m` / `{memory}Mi` without parsing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceProfile {
    /// CPU in millicores (`1000` = one core).
    pub cpu_millicores: u32,
    /// Memory in MiB.
    pub memory_mib: u64,
}

impl ResourceProfile {
    /// Builds a profile.
    pub fn new(cpu_millicores: u32, memory_mib: u64) -> Self {
        Self {
            cpu_millicores,
            memory_mib,
        }
    }

    /// Every problem with the profile on its own, empty when it is usable.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.cpu_millicores == 0 {
            problems.push("cpu_millicores must be at least 1".to_string());
        }
        if self.memory_mib == 0 {
            problems.push("memory_mib must be at least 1".to_string());
        }
        problems
    }

    /// Whether every dimension fits within `limit`.
    pub fn fits_within(&self, limit: &ResourceProfile) -> bool {
        self.cpu_millicores <= limit.cpu_millicores && self.memory_mib <= limit.memory_mib
    }
}

impl fmt::Display for ResourceProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}m CPU, {} MiB", self.cpu_millicores, self.memory_mib)
    }
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// How the controller hands one credential to the tool that needs it (R19).
///
/// Written as `{ env: GH_TOKEN }` or `{ file: .config/gh/hosts.yml }`; a file
/// path is relative to the task's private HOME (R17).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretDelivery {
    /// An environment variable of the runner process.
    Env(String),
    /// A file under the private HOME.
    File(String),
}

/// Environment variables a secret may never be delivered as: the runner's
/// own contract (R21) depends on them.
const RESERVED_ENV: [&str; 4] = ["PATH", "HOME", "USER", "SHELL"];

impl SecretDelivery {
    /// Why this delivery target cannot be used, if it cannot.
    pub fn problem(&self) -> Option<String> {
        match self {
            SecretDelivery::Env(name) => {
                let valid = name
                    .bytes()
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
                if !valid {
                    Some(format!(
                        "environment variable '{name}' must be [A-Za-z_][A-Za-z0-9_]*"
                    ))
                } else if RESERVED_ENV.contains(&name.as_str()) || name.starts_with("NARSOR_") {
                    Some(format!(
                        "environment variable '{name}' is reserved for the runner"
                    ))
                } else {
                    None
                }
            }
            SecretDelivery::File(path) => {
                let valid = !path.is_empty()
                    && path.len() <= 255
                    && !path.starts_with('/')
                    && !path.contains('\\')
                    && path
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != "..")
                    && !path.chars().any(char::is_control);
                (!valid).then(|| {
                    format!(
                        "file '{path}' must be a relative path inside the private HOME, \
                         without '.' or '..' components"
                    )
                })
            }
        }
    }
}

impl fmt::Display for SecretDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretDelivery::Env(name) => write!(f, "env {name}"),
            SecretDelivery::File(path) => write!(f, "file ~/{path}"),
        }
    }
}

/// A non-secret pointer to one credential on one backend (R19).
///
/// `key` is the backend-native locator — for Kubernetes `secret-name/key` in
/// the backend's trust namespace (KTD6) — and is safe to store, log and show.
/// The value it points at never leaves the backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SecretReference {
    /// Backend the credential is registered on.
    pub backend: BackendId,
    /// Reference name profiles and repositories use.
    pub name: String,
    /// Tool the credential is for (`gh`, `claude-code`, `git`…).
    pub tool: String,
    /// Backend-native locator of the value.
    pub key: String,
    /// How the controller delivers it.
    pub delivery: SecretDelivery,
}

/// A credential value, held only transiently by whoever materializes it.
///
/// No `Serialize`, no `Display`, and a `Debug` that prints a mask: the type
/// system keeps it out of every persisted record and API body.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place that hands it to a tool.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(********)")
    }
}

// ---------------------------------------------------------------------------
// Git identity
// ---------------------------------------------------------------------------

/// Why a Git identity was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitIdentityError {
    /// Not a full commit id.
    #[error(
        "commit '{0}' is not a full commit id (40 or 64 lowercase hex characters); \
         a remote task clones a pinned commit, never a moving ref"
    )]
    InvalidCommit(String),
}

/// What a new task clones into its empty task storage (R39).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    /// Registry id of the repository, when it came from the registry.
    pub repository: Option<String>,
    /// Remote URL handed to `git clone`.
    pub remote_url: String,
    /// The pinned commit (full SHA-1 or SHA-256 object id).
    ///
    /// Empty only for a workspace pinned at scheduler admission (see
    /// [`GitIdentity::git_ref`]); the runner then resolves the commit while
    /// cloning and records it in the task storage.
    pub commit: String,
    /// The branch or tag to resolve when `commit` is empty; the remote's
    /// `HEAD` when that is `None` too. Ignored once a commit is pinned.
    ///
    /// Resolving needs the network, so it is deliberately left to admitted
    /// provisioning rather than to task creation (R4, R6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The credential used to clone, as a reference.
    pub credential: Option<SecretReference>,
}

impl GitIdentity {
    /// Whether the commit was fixed when the task was created.
    pub fn is_pinned(&self) -> bool {
        !self.commit.is_empty()
    }

    /// Checks that `commit` is a full object id.
    pub fn check_commit(commit: &str) -> Result<(), GitIdentityError> {
        if matches!(commit.len(), 40 | 64) && is_lower_hex(commit) {
            Ok(())
        } else {
            Err(GitIdentityError::InvalidCommit(commit.to_string()))
        }
    }
}

/// The workspace source of a launched task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskWorkspace {
    /// The workflow declares no workspace.
    None,
    /// A host-local directory copied into a local backend's task storage at
    /// admitted runner initialization. It is never valid for an image-based
    /// backend, where a host path would not be portable.
    Local { path: String },
    /// A pinned Git commit.
    Git(GitIdentity),
}

// ---------------------------------------------------------------------------
// Profile and environment
// ---------------------------------------------------------------------------

/// The user profile as it was when the task was created (R16, R18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    /// Profile id.
    pub id: String,
    /// Its declared version.
    pub version: u32,
    /// SHA-256 of its whole content, so an edit that forgot to bump the
    /// version still changes the environment identity.
    pub digest: String,
    /// Allowed settings.
    pub settings: BTreeMap<String, String>,
    /// Skills, materialized from the runtime bundle.
    pub skills: Vec<String>,
    /// Tool packages.
    pub tools: Vec<String>,
}

/// Every pinned component a task's runner environment is made of (KTD5).
///
/// Built once at task creation and never re-resolved for that task: a later
/// configuration change affects only tasks created after it (R18, R42).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedEnvironment {
    /// The base image the main container runs, `None` only on a backend that
    /// runs no images (the local adapter).
    pub base_image: Option<ResolvedImage>,
    /// Per-state overrides of the base image, by state id.
    #[serde(default)]
    pub step_images: BTreeMap<String, ResolvedImage>,
    /// The Narsor runtime bundle the init container materializes (R20).
    pub runtime_bundle: Option<ResolvedImage>,
    /// Tool packages, by tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, ResolvedImage>,
    /// The user profile.
    pub profile: ResolvedProfile,
    /// Harness for agent steps that name none — pinned per task (R42).
    pub harness: String,
    /// Credentials the controller injects, as references only, by name.
    #[serde(default)]
    pub secrets: Vec<SecretReference>,
    /// `sha256:` hash of every field above (R21).
    pub identity: String,
}

impl ResolvedEnvironment {
    /// An environment that pins nothing but a harness: no image, no bundle,
    /// no tools, no profile, no secrets. What a task on the local adapter
    /// minimally needs, and a compact fixture for orchestration tests.
    pub fn harness_only(harness: impl Into<String>) -> Self {
        Self {
            base_image: None,
            step_images: BTreeMap::new(),
            runtime_bundle: None,
            tools: BTreeMap::new(),
            profile: ResolvedProfile {
                id: "none".to_string(),
                version: 0,
                digest: canonical_sha256(&serde_json::Value::Null),
                settings: BTreeMap::new(),
                skills: Vec::new(),
                tools: Vec::new(),
            },
            harness: harness.into(),
            secrets: Vec::new(),
            identity: String::new(),
        }
        .sealed()
    }

    /// Computes and stores [`Self::identity`].
    pub fn sealed(mut self) -> Self {
        self.secrets.sort();
        self.identity = self.compute_identity();
        self
    }

    /// The identity the content implies.
    pub fn compute_identity(&self) -> String {
        let mut value = serde_json::to_value(self).expect("an environment always serializes");
        if let Some(object) = value.as_object_mut() {
            object.remove("identity");
        }
        canonical_sha256(&value)
    }

    /// Whether the stored identity matches the content.
    pub fn identity_is_valid(&self) -> bool {
        self.identity == self.compute_identity()
    }

    /// The image `state` runs in: its override, else the base image.
    pub fn image_for_state(&self, state: &str) -> Option<&ResolvedImage> {
        self.step_images.get(state).or(self.base_image.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_tags_digests_and_registry_ports() {
        let pinned = ImageReference::parse(&format!("ghcr.io/acme/api@{DIGEST}")).unwrap();
        assert_eq!(pinned.repository, "ghcr.io/acme/api");
        assert_eq!(pinned.tag, None);
        assert_eq!(pinned.digest.as_ref().unwrap().as_str(), DIGEST);

        let tagged = ImageReference::parse("registry.local:5000/team/img:1.2-rc").unwrap();
        assert_eq!(tagged.repository, "registry.local:5000/team/img");
        assert_eq!(tagged.tag.as_deref(), Some("1.2-rc"));

        let both = ImageReference::parse(&format!("ubuntu:24.04@{DIGEST}")).unwrap();
        assert_eq!(both.to_string(), format!("ubuntu:24.04@{DIGEST}"));
        assert_eq!(
            both.pinned(both.digest.as_ref().unwrap()),
            format!("ubuntu@{DIGEST}")
        );
    }

    #[test]
    fn refuses_implicit_latest_options_and_bad_digests() {
        for bad in [
            "ubuntu",
            "-rm:1",
            "Ubuntu:1",
            "ubuntu:",
            "ubuntu:.x",
            "ubuntu@sha256:abc",
            "ubuntu@md5:0123",
            "a b:1",
            "",
        ] {
            assert!(ImageReference::parse(bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn a_pinned_policy_requires_a_digest() {
        assert!(matches!(
            ImageSpec::pinned("ubuntu:24.04").check(),
            Err(ImageError::Unpinned(_))
        ));
        assert!(ImageSpec::resolve("ubuntu:24.04").check().is_ok());
        assert!(ImageSpec::pinned(format!("ubuntu@{DIGEST}"))
            .check()
            .is_ok());
    }

    #[test]
    fn image_specs_read_both_spellings_and_round_trip() {
        let short: ImageSpec = serde_yaml::from_str(&format!("ubuntu@{DIGEST}")).unwrap();
        assert_eq!(short, ImageSpec::pinned(format!("ubuntu@{DIGEST}")));
        let long: ImageSpec =
            serde_yaml::from_str("reference: ubuntu:24.04\ndigest_policy: resolve\n").unwrap();
        assert_eq!(long, ImageSpec::resolve("ubuntu:24.04"));
        for spec in [short, long] {
            let text = serde_yaml::to_string(&spec).unwrap();
            assert_eq!(serde_yaml::from_str::<ImageSpec>(&text).unwrap(), spec);
        }
        assert!(serde_yaml::from_str::<ImageSpec>("reference: x:1\nbogus: 1\n").is_err());
    }

    #[test]
    fn canonical_hashing_ignores_key_order() {
        let one = serde_json::json!({ "b": 1, "a": { "y": [1, 2], "x": "t" } });
        let two: serde_json::Value =
            serde_json::from_str(r#"{"a":{"x":"t","y":[1,2]},"b":1}"#).unwrap();
        assert_eq!(canonical_sha256(&one), canonical_sha256(&two));
        assert!(canonical_sha256(&one).starts_with(SHA256_PREFIX));
        assert_ne!(
            canonical_sha256(&one),
            canonical_sha256(&serde_json::json!({ "b": 2 }))
        );
    }

    #[test]
    fn secret_delivery_targets_are_checked() {
        assert!(SecretDelivery::Env("GH_TOKEN".into()).problem().is_none());
        assert!(SecretDelivery::Env("PATH".into()).problem().is_some());
        assert!(SecretDelivery::Env("NARSOR_X".into()).problem().is_some());
        assert!(SecretDelivery::Env("1X".into()).problem().is_some());
        assert!(SecretDelivery::File(".config/gh/hosts.yml".into())
            .problem()
            .is_none());
        for bad in ["/etc/passwd", "../x", "a//b", "", "./x"] {
            assert!(
                SecretDelivery::File(bad.into()).problem().is_some(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_secret_value_never_prints() {
        let value = SecretValue::new("hunter2");
        assert!(!format!("{value:?}").contains("hunter2"));
        assert_eq!(value.expose(), "hunter2");
    }

    #[test]
    fn commits_must_be_full_object_ids() {
        assert!(GitIdentity::check_commit(&"a".repeat(40)).is_ok());
        assert!(GitIdentity::check_commit(&"b".repeat(64)).is_ok());
        assert!(GitIdentity::check_commit("main").is_err());
        assert!(GitIdentity::check_commit(&"A".repeat(40)).is_err());
    }
}
