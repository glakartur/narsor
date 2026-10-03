//! OIDC access-token verification for network mode (R35, KTD6).
//!
//! The server is a resource server: it never runs a login flow and holds no
//! client secret. A client obtains a token from the configured issuer and
//! presents it as `Authorization: Bearer …`; [`OidcVerifier::verify`] accepts
//! it only when every one of these holds:
//!
//! * the header names an **asymmetric** algorithm on the allow-list — `none`
//!   and every HMAC algorithm are refused before any key is looked at, which
//!   closes the classic "verify an HS256 token with the public key as the
//!   secret" confusion;
//! * the issuer's JWKS holds a signing key with the token's `kid` whose own
//!   `alg` (when it states one) and key type match the header — a key is
//!   never used for an algorithm it was not published for;
//! * the signature verifies;
//! * `iss` equals the configured issuer **exactly**, `aud` contains the
//!   configured audience, `exp` has not passed, `nbf` has, and `iat` is not
//!   in the future — each with a small leeway for clock skew;
//! * `sub` is present.
//!
//! The identity is the immutable pair `(iss, sub)` — never an e-mail or a
//! display name, which an issuer may reassign.
//!
//! # Keys: discovery, caching and rotation
//!
//! The JWKS location comes from the issuer's discovery document, whose own
//! `issuer` must equal the configured one. Keys are cached; an unknown `kid`
//! (the issuer rotated) triggers a refetch, at most once per
//! [`OidcSettings::min_refresh_interval`] so a stream of tokens with made-up
//! key ids cannot turn the server into a request amplifier against the
//! issuer. The cache is refreshed anyway after [`OidcSettings::max_key_age`],
//! so a key the issuer withdrew stops verifying. Refreshes are serialized:
//! concurrent requests wait for one fetch instead of each starting their own.

use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::Url;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::orchestration::OwnerRef;

use super::settings::{is_loopback_host, parse_issuer};

/// The only algorithms a token may be signed with: asymmetric ones.
pub const ALLOWED_ALGORITHMS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// Prefix of every [`OwnerRef`] an OIDC identity maps to.
const OWNER_REF_PREFIX: &str = "oidc:";

/// The trusted issuer and how strictly it is followed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcSettings {
    /// The issuer identifier, matched exactly against `iss`.
    pub issuer: String,
    /// The audience every token must name.
    pub audience: String,
    /// Clock-skew allowance for `exp`, `nbf` and `iat`.
    pub leeway: Duration,
    /// Shortest time between two key refetches caused by unknown key ids.
    pub min_refresh_interval: Duration,
    /// Cached keys older than this are refetched before use.
    pub max_key_age: Duration,
    /// Bound of one request to the issuer.
    pub http_timeout: Duration,
}

impl OidcSettings {
    /// The issuer and audience with the default policy: 30 s leeway, at most
    /// one unknown-key refetch a minute, keys refreshed hourly.
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            leeway: Duration::from_secs(30),
            min_refresh_interval: Duration::from_secs(60),
            max_key_age: Duration::from_secs(3600),
            http_timeout: Duration::from_secs(5),
        }
    }
}

/// A verified user: the immutable `(issuer, subject)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OidcIdentity {
    /// The issuer.
    pub issuer: String,
    /// The subject, unique and never reassigned within the issuer.
    pub subject: String,
}

impl OidcIdentity {
    /// The pair.
    pub fn new(issuer: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
        }
    }

    /// The owner reference tasks of this identity are stored under.
    ///
    /// `#` separates the two because an issuer identifier never contains a
    /// fragment (checked by [`parse_issuer`]), so the first `#` is
    /// unambiguous whatever the subject holds.
    pub fn owner_ref(&self) -> OwnerRef {
        OwnerRef::new(format!(
            "{OWNER_REF_PREFIX}{}#{}",
            self.issuer, self.subject
        ))
    }
}

/// A token that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    /// Who presented it.
    pub identity: OidcIdentity,
    /// When it stops being valid: a stream it opened ends then.
    pub expires_at: DateTime<Utc>,
}

/// Why a token was refused. Only ever logged — the client sees a bare `401`.
#[derive(Debug, thiserror::Error)]
pub enum TokenRejected {
    /// Not a JWT, or a header that names no supported algorithm (`none`).
    #[error("malformed token: {0}")]
    Malformed(String),
    /// A symmetric or otherwise disallowed algorithm.
    #[error("algorithm {0:?} is not allowed")]
    DisallowedAlgorithm(Algorithm),
    /// No published signing key matches the token's key id.
    #[error("no signing key matches the token")]
    UnknownKey,
    /// The key exists but was not published for this algorithm.
    #[error("the signing key does not match the token's algorithm")]
    KeyMismatch,
    /// Signature or claims did not verify.
    #[error("invalid token: {0}")]
    Invalid(String),
    /// The issuer's keys could not be fetched.
    #[error("issuer keys unavailable: {0}")]
    KeysUnavailable(String),
}

/// The registered claims the server reads.
#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: i64,
    #[serde(default)]
    iat: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

#[derive(Default)]
struct KeyCache {
    keys: Vec<Jwk>,
    fetched_at: Option<Instant>,
    last_attempt: Option<Instant>,
}

/// Verifies access tokens of one issuer.
pub struct OidcVerifier {
    settings: OidcSettings,
    discovery_url: Url,
    http: reqwest::Client,
    cache: Mutex<KeyCache>,
}

impl std::fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// A verifier for `settings.issuer`. Nothing is fetched yet: the
    /// issuer being briefly unreachable must not stop the daemon from
    /// starting, only fail the requests that need its keys.
    pub fn new(settings: OidcSettings) -> Result<Self, String> {
        let issuer = parse_issuer(&settings.issuer)?;
        let discovery_url = Url::parse(&format!(
            "{}/.well-known/openid-configuration",
            issuer.as_str().trim_end_matches('/')
        ))
        .map_err(|error| format!("cannot build the discovery URL: {error}"))?;
        // No redirects: the discovery document and the keys come from the
        // URLs the issuer names, not from wherever a response points.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(settings.http_timeout)
            .build()
            .map_err(|error| format!("cannot build the HTTP client: {error}"))?;
        Ok(Self {
            settings,
            discovery_url,
            http,
            cache: Mutex::new(KeyCache::default()),
        })
    }

    /// The policy in force.
    pub fn settings(&self) -> &OidcSettings {
        &self.settings
    }

    /// Verifies `token` and returns who presented it.
    pub async fn verify(&self, token: &str) -> Result<VerifiedToken, TokenRejected> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|error| TokenRejected::Malformed(error.to_string()))?;
        if !ALLOWED_ALGORITHMS.contains(&header.alg) {
            return Err(TokenRejected::DisallowedAlgorithm(header.alg));
        }
        let key = self.key_for(header.kid.as_deref(), header.alg).await?;

        let leeway = self.settings.leeway.as_secs();
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.settings.issuer]);
        validation.set_audience(&[&self.settings.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = leeway;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .map_err(|error| TokenRejected::Invalid(error.to_string()))?;
        let claims = data.claims;

        let now = Utc::now().timestamp();
        if claims.iat.is_some_and(|iat| iat > now + leeway as i64) {
            return Err(TokenRejected::Invalid("issued in the future".to_string()));
        }
        if claims.sub.trim().is_empty() {
            return Err(TokenRejected::Invalid("empty subject".to_string()));
        }
        let expires_at = Utc
            .timestamp_opt(claims.exp, 0)
            .single()
            .ok_or_else(|| TokenRejected::Invalid("expiry out of range".to_string()))?;
        Ok(VerifiedToken {
            identity: OidcIdentity::new(claims.iss, claims.sub),
            expires_at,
        })
    }

    /// The decoding key for `kid`/`alg`, refreshing the cache when it is
    /// old, or once per interval when the key id is unknown.
    async fn key_for(
        &self,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<DecodingKey, TokenRejected> {
        let mut cache = self.cache.lock().await;
        let expired = cache
            .fetched_at
            .is_none_or(|at| at.elapsed() >= self.settings.max_key_age);
        if expired && self.may_refresh(&cache) {
            if let Err(error) = self.refresh(&mut cache).await {
                tracing::warn!(error = %error, "oidc: could not refresh the issuer's keys");
                if cache.keys.is_empty() {
                    return Err(TokenRejected::KeysUnavailable(error));
                }
            }
        }
        match select_key(&cache.keys, kid, alg) {
            Err(TokenRejected::UnknownKey) if self.may_refresh(&cache) => {
                // The issuer may have rotated: look once more.
                self.refresh(&mut cache)
                    .await
                    .map_err(TokenRejected::KeysUnavailable)?;
                select_key(&cache.keys, kid, alg)
            }
            other => other,
        }
    }

    fn may_refresh(&self, cache: &KeyCache) -> bool {
        cache
            .last_attempt
            .is_none_or(|at| at.elapsed() >= self.settings.min_refresh_interval)
    }

    async fn refresh(&self, cache: &mut KeyCache) -> Result<(), String> {
        cache.last_attempt = Some(Instant::now());
        let document: DiscoveryDocument = self.fetch_json(self.discovery_url.clone()).await?;
        if document.issuer != self.settings.issuer {
            return Err(format!(
                "the discovery document names issuer '{}', not the configured '{}'",
                document.issuer, self.settings.issuer
            ));
        }
        let jwks_uri =
            Url::parse(&document.jwks_uri).map_err(|error| format!("invalid jwks_uri: {error}"))?;
        let plain_allowed = self.discovery_url.scheme() == "http" && is_loopback_host(&jwks_uri);
        if jwks_uri.scheme() != "https" && !plain_allowed {
            return Err(format!("jwks_uri '{jwks_uri}' is not HTTPS"));
        }
        let set: JwkSet = self.fetch_json(jwks_uri).await?;
        cache.keys = set.keys;
        cache.fetched_at = Some(Instant::now());
        tracing::debug!(keys = cache.keys.len(), "oidc: refreshed the issuer's keys");
        Ok(())
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: Url) -> Result<T, String> {
        let response = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|error| format!("GET {url}: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("GET {url}: status {}", response.status()));
        }
        response
            .json::<T>()
            .await
            .map_err(|error| format!("GET {url}: {error}"))
    }
}

/// The one published signing key that may verify a token with `kid`/`alg`.
///
/// Symmetric (`oct`) keys and keys published for encryption are never
/// candidates. Without a `kid`, the key must be the only one that fits.
fn select_key(
    keys: &[Jwk],
    kid: Option<&str>,
    alg: Algorithm,
) -> Result<DecodingKey, TokenRejected> {
    let mut candidates = keys.iter().filter(|jwk| {
        !matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_))
            && !matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption))
            && match kid {
                Some(kid) => jwk.common.key_id.as_deref() == Some(kid),
                None => true,
            }
    });
    let jwk = candidates.next().ok_or(TokenRejected::UnknownKey)?;
    if kid.is_none() && candidates.next().is_some() {
        // Ambiguous: a token must say which key signed it.
        return Err(TokenRejected::UnknownKey);
    }
    if let Some(published) = jwk.common.key_algorithm {
        // Both enums name the JOSE algorithms alike ("ES256", "EdDSA").
        if format!("{published:?}") != format!("{alg:?}") {
            return Err(TokenRejected::KeyMismatch);
        }
    }
    let key = DecodingKey::from_jwk(jwk).map_err(|_| TokenRejected::KeyMismatch)?;
    if key.family() != alg.family() {
        return Err(TokenRejected::KeyMismatch);
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_ref_separates_issuer_and_subject_unambiguously() {
        let identity = OidcIdentity::new("https://issuer.example/realm", "a#b");
        assert_eq!(
            identity.owner_ref(),
            OwnerRef::new("oidc:https://issuer.example/realm#a#b")
        );
    }

    #[test]
    fn hmac_and_unknown_algorithms_are_not_on_the_allow_list() {
        for refused in [Algorithm::HS256, Algorithm::HS384, Algorithm::HS512] {
            assert!(!ALLOWED_ALGORITHMS.contains(&refused));
        }
    }
}
