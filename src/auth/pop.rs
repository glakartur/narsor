//! Controller proof-of-possession (R37, KTD6).
//!
//! # Why signed requests and not mTLS or bearer secrets
//!
//! A bearer secret (U2's `nsc.` tokens) authenticates whoever copies it, for
//! as long as it lives. mTLS would bind the credential to a key, but the
//! server does not terminate TLS itself in network mode: the reverse proxy
//! does, and a client certificate would end there, reaching the server only
//! as a header the proxy asserts. A signature over each request survives TLS
//! termination unchanged and is checked by the server itself.
//!
//! # The scheme
//!
//! * The controller generates an Ed25519 key pair and never sends the
//!   private half anywhere.
//! * **Enrollment** binds the public key: the controller redeems the
//!   operator's one-time, expiry-bound code together with its public key and
//!   a signature over both ([`enrollment_message`]), proving it holds the key.
//! * **Every request** carries `Authorization: Narsor-PoP
//!   {controller_id}.{unix_seconds}.{nonce}.{signature}`, the signature
//!   covering the controller id, method, path with query, timestamp, nonce
//!   and a SHA-256 of the body ([`request_message`]). The server accepts a
//!   timestamp within [`MAX_CLOCK_SKEW`] of its own clock and each nonce only
//!   once within that window, so a captured request cannot be replayed —
//!   not even against another path or with another body.
//! * **Rotation** replaces the public key: a request signed with the current
//!   key carries the new public key and a signature by the new key
//!   ([`rotation_message`]). The old key stops working at once.
//! * **Revocation** marks the identity revoked; no key of it verifies again.
//!
//! The controller persists its identity as one opaque credential string
//! (`nsk.{controller_id}.{seed}`, [`ControllerCredential`]), mounted `0600`
//! like any other secret.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

/// The `Authorization` scheme of a signed controller request.
pub const POP_SCHEME: &str = "Narsor-PoP";

/// How far a request's timestamp may be from the server's clock, either way.
pub const MAX_CLOCK_SKEW: Duration = Duration::seconds(60);

/// Prefix of a persisted controller credential.
const CREDENTIAL_PREFIX: &str = "nsk";

/// Domain separation of every signed message, so a signature made for one
/// purpose never verifies for another.
const REQUEST_CONTEXT: &str = "narsor-controller-request-v1";
const ENROLL_CONTEXT: &str = "narsor-controller-enroll-v1";
const ROTATE_CONTEXT: &str = "narsor-controller-rotate-v1";

/// Most nonces remembered per controller; a controller exceeding it within
/// one skew window is refused until older nonces age out.
const MAX_NONCES_PER_CONTROLLER: usize = 50_000;

/// A controller's signing key. `Debug` is redacted.
#[derive(Clone)]
pub struct ControllerKey {
    signing: SigningKey,
}

impl std::fmt::Debug for ControllerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

impl ControllerKey {
    /// A new key from the operating system's random source.
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut rand_core::OsRng),
        }
    }

    /// The public key, base64url without padding, as enrollment sends it.
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.verifying_key().as_bytes())
    }

    /// A base64url signature of `message`.
    pub fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.sign(message).to_bytes())
    }

    fn seed(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.to_bytes())
    }

    fn from_seed(seed: &str) -> Option<Self> {
        let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(seed).ok()?.try_into().ok()?;
        Some(Self {
            signing: SigningKey::from_bytes(&bytes),
        })
    }
}

/// A controller identity as its holder persists it: the id the server
/// assigned and the key only the holder knows.
#[derive(Clone)]
pub struct ControllerCredential {
    controller_id: String,
    key: ControllerKey,
}

impl std::fmt::Debug for ControllerCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerCredential")
            .field("controller_id", &self.controller_id)
            .finish_non_exhaustive()
    }
}

impl ControllerCredential {
    /// The credential of `controller_id` holding `key`.
    pub fn new(controller_id: impl Into<String>, key: ControllerKey) -> Self {
        Self {
            controller_id: controller_id.into(),
            key,
        }
    }

    /// Reads a persisted credential; `None` when it is not one.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text
            .trim()
            .strip_prefix(CREDENTIAL_PREFIX)?
            .strip_prefix('.')?;
        let (controller_id, seed) = rest.rsplit_once('.')?;
        if controller_id.is_empty() || controller_id.contains('.') {
            return None;
        }
        Some(Self::new(controller_id, ControllerKey::from_seed(seed)?))
    }

    /// The credential as stored: a secret, handle like one.
    pub fn expose(&self) -> String {
        format!(
            "{CREDENTIAL_PREFIX}.{}.{}",
            self.controller_id,
            self.key.seed()
        )
    }

    /// The identity.
    pub fn controller_id(&self) -> &str {
        &self.controller_id
    }

    /// The key.
    pub fn key(&self) -> &ControllerKey {
        &self.key
    }

    /// The `Authorization` header value for one request, signed now.
    pub fn authorization(&self, method: &str, path_and_query: &str, body: &[u8]) -> String {
        let timestamp = Utc::now().timestamp();
        let nonce = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
        let message = request_message(
            &self.controller_id,
            method,
            path_and_query,
            timestamp,
            &nonce,
            &body_digest(body),
        );
        format!(
            "{POP_SCHEME} {}.{timestamp}.{nonce}.{}",
            self.controller_id,
            self.key.sign(&message)
        )
    }
}

/// What enrollment signs: the code and the key it binds.
pub fn enrollment_message(enrollment_code: &str, public_key: &str) -> Vec<u8> {
    format!("{ENROLL_CONTEXT}\n{enrollment_code}\n{public_key}").into_bytes()
}

/// What a key rotation's new key signs.
pub fn rotation_message(controller_id: &str, public_key: &str) -> Vec<u8> {
    format!("{ROTATE_CONTEXT}\n{controller_id}\n{public_key}").into_bytes()
}

/// What one request's signature covers.
pub fn request_message(
    controller_id: &str,
    method: &str,
    path_and_query: &str,
    timestamp: i64,
    nonce: &str,
    body_digest: &str,
) -> Vec<u8> {
    format!(
        "{REQUEST_CONTEXT}\n{controller_id}\n{}\n{path_and_query}\n{timestamp}\n{nonce}\n{body_digest}",
        method.to_ascii_uppercase()
    )
    .into_bytes()
}

/// Hex SHA-256 of a request body.
pub fn body_digest(body: &[u8]) -> String {
    Sha256::digest(body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether `signature` (base64url) by `public_key` (base64url) verifies
/// `message`. Malformed input is simply `false`.
pub fn verify(public_key: &str, message: &[u8], signature: &str) -> bool {
    let Some(key) = decode_public_key(public_key) else {
        return false;
    };
    let Some(signature) = URL_SAFE_NO_PAD
        .decode(signature)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
    else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

/// Whether `public_key` is a well-formed Ed25519 public key.
pub fn is_public_key(public_key: &str) -> bool {
    decode_public_key(public_key).is_some()
}

fn decode_public_key(public_key: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = URL_SAFE_NO_PAD.decode(public_key).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

/// A request as the server saw it, for checking its proof.
#[derive(Debug, Clone, Copy)]
pub struct SignedRequest<'a> {
    /// The `Authorization` header value, if any.
    pub authorization: Option<&'a str>,
    /// The HTTP method.
    pub method: &'a str,
    /// Path and query as received.
    pub path_and_query: &'a str,
    /// The body bytes.
    pub body: &'a [u8],
}

/// The parts of a `Narsor-PoP` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    /// The claimed identity.
    pub controller_id: String,
    /// When the controller signed.
    pub timestamp: i64,
    /// The one-time nonce.
    pub nonce: String,
    /// The signature.
    pub signature: String,
}

impl Proof {
    /// Parses an `Authorization` header value; `None` for anything else.
    pub fn parse(authorization: &str) -> Option<Self> {
        let value = authorization.strip_prefix(POP_SCHEME)?.strip_prefix(' ')?;
        let mut parts = value.split('.');
        let proof = Self {
            controller_id: parts.next()?.to_string(),
            timestamp: parts.next()?.parse().ok()?,
            nonce: parts.next()?.to_string(),
            signature: parts.next()?.to_string(),
        };
        let well_formed = parts.next().is_none()
            && !proof.controller_id.is_empty()
            && (16..=64).contains(&proof.nonce.len());
        well_formed.then_some(proof)
    }

    /// Whether the proof is fresh, signed by `public_key` and covers
    /// `request`. The nonce is checked separately, after the signature, so
    /// an unsigned request cannot fill the replay memory.
    pub fn verifies(
        &self,
        public_key: &str,
        request: &SignedRequest<'_>,
        now: DateTime<Utc>,
    ) -> bool {
        let skew = (now.timestamp() - self.timestamp).abs();
        if skew > MAX_CLOCK_SKEW.num_seconds() {
            return false;
        }
        let message = request_message(
            &self.controller_id,
            request.method,
            request.path_and_query,
            self.timestamp,
            &self.nonce,
            &body_digest(request.body),
        );
        verify(public_key, &message, &self.signature)
    }
}

/// Nonces seen within the skew window, per controller.
#[derive(Debug, Default)]
pub struct ReplayGuard {
    seen: Mutex<HashMap<String, HashMap<String, i64>>>,
}

impl ReplayGuard {
    /// Records `proof`'s nonce; `false` when it was already used (a replay)
    /// or the controller's memory is full.
    pub fn first_use(&self, proof: &Proof, now: DateTime<Utc>) -> bool {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let nonces = seen.entry(proof.controller_id.clone()).or_default();
        let horizon = now.timestamp() - MAX_CLOCK_SKEW.num_seconds();
        nonces.retain(|_, at| *at >= horizon);
        if nonces.contains_key(&proof.nonce) || nonces.len() >= MAX_NONCES_PER_CONTROLLER {
            return false;
        }
        // Remembered until its timestamp leaves the window, after which the
        // timestamp check alone refuses it.
        nonces.insert(
            proof.nonce.clone(),
            proof.timestamp + MAX_CLOCK_SKEW.num_seconds(),
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(authorization: &'a str, body: &'a [u8]) -> SignedRequest<'a> {
        SignedRequest {
            authorization: Some(authorization),
            method: "POST",
            path_and_query: "/controller/v1/reports",
            body,
        }
    }

    #[test]
    fn a_signed_request_verifies_once_and_only_as_signed() {
        let key = ControllerKey::generate();
        let credential = ControllerCredential::new("ctl-1", key.clone());
        let header = credential.authorization("POST", "/controller/v1/reports", b"{}");
        let proof = Proof::parse(&header).expect("parses");
        let now = Utc::now();

        assert!(proof.verifies(&key.public_key(), &request(&header, b"{}"), now));
        assert!(!proof.verifies(&key.public_key(), &request(&header, b"{\"x\":1}"), now));
        let other_path = SignedRequest {
            path_and_query: "/controller/v1/acks",
            ..request(&header, b"{}")
        };
        assert!(!proof.verifies(&key.public_key(), &other_path, now));
        assert!(!proof.verifies(
            &ControllerKey::generate().public_key(),
            &request(&header, b"{}"),
            now
        ));
        assert!(!proof.verifies(
            &key.public_key(),
            &request(&header, b"{}"),
            now + Duration::minutes(5)
        ));

        let guard = ReplayGuard::default();
        assert!(guard.first_use(&proof, now));
        assert!(!guard.first_use(&proof, now), "a replayed nonce is refused");
    }

    #[test]
    fn a_credential_round_trips_and_keeps_its_key() {
        let credential = ControllerCredential::new("ctl-abc", ControllerKey::generate());
        let parsed = ControllerCredential::parse(&credential.expose()).expect("parses");
        assert_eq!(parsed.controller_id(), "ctl-abc");
        assert_eq!(parsed.key().public_key(), credential.key().public_key());
        assert!(ControllerCredential::parse("nsc.ctl-abc.secret").is_none());
    }
}
