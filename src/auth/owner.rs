//! The installation owner of a network-mode control plane (R31-R33, R38).
//!
//! A network installation belongs to exactly one OIDC identity, bound in one
//! of two ways:
//!
//! * **by deployment configuration** — `owner_subject` names the subject of
//!   the configured issuer; the binding is (re)written at every start, so the
//!   configuration stays the authority;
//! * **by a one-time claim** — with no owner configured or recorded, the
//!   daemon generates a claim code at startup, prints it and stores it `0600`
//!   for the operator. The first identity with a valid token that presents
//!   it becomes the owner; the code is then destroyed. Claims are serialized,
//!   so of concurrent claims exactly one wins.
//!
//! Every other identity gets no access at all.
//!
//! # Built to change later (R33)
//!
//! The record carries a revision and says how it was bound. Changing the
//! owner is [`OwnerStore::rebind`]: a new revision, the old identity refused
//! from the next request on (and its open streams closed by the gate). No
//! transfer API is exposed in this version; the deployment configuration is
//! the only way to rebind, and tasks keep the owner they were created by.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::controller_protocol::credentials::constant_time_eq;
use crate::persistence::{read_json, write_json_atomic_with_mode, PersistenceError, StatePaths};

use super::oidc::OidcIdentity;

/// Current schema version of [`OwnerRecord`].
pub const OWNER_RECORD_VERSION: u32 = 1;

/// Current schema version of the claim record.
const CLAIM_RECORD_VERSION: u32 = 1;

/// Mode of every file this module writes: the claim code is a credential,
/// and the owner record says whom to impersonate.
const AUTH_FILE_MODE: u32 = 0o600;

/// Prefix of a claim code, so an operator recognises one.
const CLAIM_CODE_PREFIX: &str = "nsclaim";

/// How an owner came to be bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerBindingSource {
    /// `owner_subject` in the deployment configuration.
    Configuration,
    /// The one-time claim code.
    Claim,
}

/// The durable record of the installation owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    /// Schema version.
    pub record_version: u32,
    /// The owner's issuer.
    pub issuer: String,
    /// The owner's subject.
    pub subject: String,
    /// How the binding was made.
    pub bound_by: OwnerBindingSource,
    /// When.
    pub bound_at: DateTime<Utc>,
    /// Grows with every rebinding, so a change of owner is never silent.
    pub revision: u64,
}

impl OwnerRecord {
    /// The owner's identity.
    pub fn identity(&self) -> OidcIdentity {
        OidcIdentity::new(&self.issuer, &self.subject)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimRecord {
    record_version: u32,
    code_digest: String,
    created_at: DateTime<Utc>,
}

/// A freshly generated claim code. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaimCode(String);

impl ClaimCode {
    fn generate() -> Self {
        Self(format!(
            "{CLAIM_CODE_PREFIX}-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    /// The code as the operator hands it on.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClaimCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaimCode(<redacted>)")
    }
}

/// The ownership situation the daemon starts in.
#[derive(Debug)]
pub enum StartupOwnership {
    /// The installation has an owner.
    Owned(OwnerRecord),
    /// Nobody owns it yet; the code is printed and stored at `code_file`.
    Unclaimed {
        /// The one-time code.
        code: ClaimCode,
        /// Where the operator can read it again.
        code_file: std::path::PathBuf,
    },
}

/// What a claim achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The caller is now the owner.
    Claimed(OwnerRecord),
    /// The caller already was the owner; nothing changed.
    AlreadyOwner(OwnerRecord),
    /// Refused: a wrong or spent code, or another owner. Deliberately one
    /// variant, so the response cannot tell which.
    Refused,
}

/// A failure of the ownership records themselves.
#[derive(Debug, thiserror::Error)]
pub enum OwnerStoreError {
    /// Reading or writing failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The record is newer than this binary understands.
    #[error("the owner record has version {0}, newer than this version of narsor supports")]
    UnsupportedVersion(u32),
    /// The recorded owner belongs to another issuer than the configured one,
    /// so nobody could ever act as the owner.
    #[error(
        "the installation is owned by an identity of issuer '{recorded}', but oidc_issuer is \
         '{configured}'; set owner_subject to rebind the owner to the new issuer"
    )]
    IssuerMismatch {
        /// The recorded owner's issuer.
        recorded: String,
        /// The configured issuer.
        configured: String,
    },
}

struct OwnerState {
    owner: Option<OwnerRecord>,
    claim_digest: Option<String>,
}

/// The installation owner, in memory and on disk.
///
/// Every request consults the in-memory copy; every change is written
/// before it takes effect there, so a crash never leaves the process
/// believing in an owner the disk does not know.
pub struct OwnerStore {
    paths: StatePaths,
    issuer: String,
    state: Mutex<OwnerState>,
}

impl std::fmt::Debug for OwnerStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerStore")
            .field("issuer", &self.issuer)
            .field("owner", &self.owner())
            .finish_non_exhaustive()
    }
}

impl OwnerStore {
    /// Opens the ownership records under `paths` for `issuer`.
    ///
    /// With `configured_subject`, that identity is (re)bound as the owner.
    /// Without it, a recorded owner stays; with neither, a new claim code is
    /// generated — a restart invalidates the previous one.
    pub fn open(
        paths: &StatePaths,
        issuer: &str,
        configured_subject: Option<&str>,
    ) -> Result<(Self, StartupOwnership), OwnerStoreError> {
        let store = Self {
            paths: paths.clone(),
            issuer: issuer.to_string(),
            state: Mutex::new(OwnerState {
                owner: None,
                claim_digest: None,
            }),
        };
        let recorded = store.read_owner()?;
        let startup = match (configured_subject, recorded) {
            (Some(subject), recorded) => {
                let identity = OidcIdentity::new(issuer, subject);
                let record = match recorded {
                    Some(record) if record.identity() == identity => record,
                    _ => store.rebind(&identity, OwnerBindingSource::Configuration)?,
                };
                store.lock().owner = Some(record.clone());
                store.remove_claim()?;
                StartupOwnership::Owned(record)
            }
            (None, Some(record)) => {
                if record.issuer != issuer {
                    return Err(OwnerStoreError::IssuerMismatch {
                        recorded: record.issuer,
                        configured: issuer.to_string(),
                    });
                }
                store.lock().owner = Some(record.clone());
                store.remove_claim()?;
                StartupOwnership::Owned(record)
            }
            (None, None) => {
                let code = store.issue_claim()?;
                StartupOwnership::Unclaimed {
                    code,
                    code_file: paths.claim_code_file(),
                }
            }
        };
        Ok((store, startup))
    }

    /// The current owner, if the installation has one.
    pub fn owner(&self) -> Option<OwnerRecord> {
        self.lock().owner.clone()
    }

    /// Whether `identity` is the installation owner right now.
    pub fn is_owner(&self, identity: &OidcIdentity) -> bool {
        self.lock()
            .owner
            .as_ref()
            .is_some_and(|owner| owner.identity() == *identity)
    }

    /// Whether somebody owns the installation.
    pub fn is_claimed(&self) -> bool {
        self.lock().owner.is_some()
    }

    /// Lets `identity` claim the unowned installation with `code`.
    ///
    /// Serialized by the store's lock: the first valid claim writes the
    /// owner and destroys the code before the lock is released, so every
    /// concurrent claim after it — the right code or not — is refused.
    pub fn claim(
        &self,
        identity: &OidcIdentity,
        code: &str,
    ) -> Result<ClaimOutcome, OwnerStoreError> {
        let mut state = self.lock();
        if let Some(owner) = &state.owner {
            return Ok(if owner.identity() == *identity {
                ClaimOutcome::AlreadyOwner(owner.clone())
            } else {
                ClaimOutcome::Refused
            });
        }
        let valid = state
            .claim_digest
            .as_deref()
            .is_some_and(|stored| constant_time_eq(digest(code).as_bytes(), stored.as_bytes()));
        if !valid || identity.issuer != self.issuer {
            return Ok(ClaimOutcome::Refused);
        }
        let record = self.write_owner(identity, OwnerBindingSource::Claim, None)?;
        state.owner = Some(record.clone());
        state.claim_digest = None;
        drop(state);
        self.remove_claim()?;
        Ok(ClaimOutcome::Claimed(record))
    }

    /// Binds `identity` as the owner, replacing any previous one (R33).
    ///
    /// The previous owner is refused from the next request on. The only
    /// caller in this version is startup with `owner_subject`; a future
    /// transfer flow is meant to go through here too.
    pub fn rebind(
        &self,
        identity: &OidcIdentity,
        source: OwnerBindingSource,
    ) -> Result<OwnerRecord, OwnerStoreError> {
        let previous = self.read_owner()?;
        if let Some(previous) = &previous {
            tracing::warn!(
                previous_subject = %previous.subject,
                subject = %identity.subject,
                "auth: the installation owner changes"
            );
        }
        let record = self.write_owner(identity, source, previous.as_ref())?;
        let mut state = self.lock();
        state.owner = Some(record.clone());
        state.claim_digest = None;
        Ok(record)
    }

    fn write_owner(
        &self,
        identity: &OidcIdentity,
        source: OwnerBindingSource,
        previous: Option<&OwnerRecord>,
    ) -> Result<OwnerRecord, OwnerStoreError> {
        let record = OwnerRecord {
            record_version: OWNER_RECORD_VERSION,
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
            bound_by: source,
            bound_at: Utc::now(),
            revision: previous.map_or(1, |previous| previous.revision + 1),
        };
        write_json_atomic_with_mode(&self.paths.owner_file(), &record, AUTH_FILE_MODE)?;
        Ok(record)
    }

    fn read_owner(&self) -> Result<Option<OwnerRecord>, OwnerStoreError> {
        let path = self.paths.owner_file();
        if !path.exists() {
            return Ok(None);
        }
        let record: OwnerRecord = read_json(&path)?;
        if record.record_version > OWNER_RECORD_VERSION {
            return Err(OwnerStoreError::UnsupportedVersion(record.record_version));
        }
        Ok(Some(record))
    }

    fn issue_claim(&self) -> Result<ClaimCode, OwnerStoreError> {
        let code = ClaimCode::generate();
        let record = ClaimRecord {
            record_version: CLAIM_RECORD_VERSION,
            code_digest: digest(code.expose()),
            created_at: Utc::now(),
        };
        write_json_atomic_with_mode(&self.paths.claim_file(), &record, AUTH_FILE_MODE)?;
        crate::persistence::write_atomic_with_mode(
            &self.paths.claim_code_file(),
            format!("{}\n", code.expose()).as_bytes(),
            AUTH_FILE_MODE,
        )?;
        self.lock().claim_digest = Some(record.code_digest);
        Ok(code)
    }

    fn remove_claim(&self) -> Result<(), OwnerStoreError> {
        for path in [self.paths.claim_file(), self.paths.claim_code_file()] {
            remove_if_present(&path)?;
        }
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, OwnerState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn remove_if_present(path: &Path) -> Result<(), OwnerStoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PersistenceError::io(path, error).into()),
    }
}

fn digest(code: &str) -> String {
    let hash = Sha256::digest(code.as_bytes());
    let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}
