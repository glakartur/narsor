//! Who may talk to the control plane, and how they prove it (U7: R31-R38,
//! KTD6).
//!
//! # Two explicit modes
//!
//! | | trusted | network |
//! | --- | --- | --- |
//! | for | one person on one machine (R36) | a shared, reachable control plane (R35) |
//! | listens on | loopback only — a wildcard, IPv4-mapped or non-loopback bind refuses to start | any address, behind a TLS-terminating proxy |
//! | users prove | a random per-process token from the `0600` discovery file | an OIDC access token of the configured issuer and audience |
//! | owner | the local token holder | one OIDC identity, bound by config or a one-time claim (R38) |
//!
//! The mode is a setting (`server_mode`), never inferred: a misconfiguration
//! stops the daemon with a message naming every problem
//! ([`SecuritySettings::validate`]).
//!
//! # Two trust planes (KTD6)
//!
//! Users are authenticated by the owner gate ([`gate`]); environment
//! controllers never use a user token — each has its own revocable identity
//! scoped to one backend and proves every request with a key only it holds
//! ([`pop`]); runners hold short-lived, incarnation-bound credentials issued
//! by their controller (`crate::controller_protocol::credentials`).
//!
//! * [`settings`] — the configuration and its cross-checks;
//! * [`oidc`] — token verification, discovery and key rotation;
//! * [`owner`] — the installation owner and its one-time claim;
//! * [`gate`] — the HTTP gates, proxy policy, CORS and stream expiry;
//! * [`pop`] — controller proof-of-possession.

pub mod gate;
pub mod oidc;
pub mod owner;
pub mod pop;
pub mod settings;

pub use gate::{
    auth_routes, ClaimRequest, NetworkAuth, Operator, OwnerAuth, OwnerView, Principal,
    WhoAmIResponse,
};
pub use oidc::{OidcIdentity, OidcSettings, OidcVerifier, TokenRejected, VerifiedToken};
pub use owner::{
    ClaimCode, ClaimOutcome, OwnerBindingSource, OwnerRecord, OwnerStore, OwnerStoreError,
    StartupOwnership,
};
pub use pop::{ControllerCredential, ControllerKey};
pub use settings::{
    parse_bind, parse_issuer, parse_origin, parse_proxy, resolve_bind, CorsPolicy, NetworkPlan,
    ProxyPolicy, SecurityConfigError, SecurityPlan, SecuritySettings, ServerMode, TlsTermination,
};
