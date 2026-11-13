//! Wire types shared by Narsor clients and services.

/// The version of Narsor every binary reports (`--version`).
///
/// A release build sets `NARSOR_BUILD_VERSION` to the release tag without its
/// leading `v` (`.github/workflows/release.yml`, `dist/package-linux.sh`);
/// any other build reports the version of the crate. Cargo rebuilds this
/// crate when the variable changes.
pub const VERSION: &str = match option_env!("NARSOR_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod api;
pub mod chat;
pub mod controller;
pub mod display;
pub mod event;
pub mod harness;
pub mod identifier;
pub mod launch;
pub mod panel;
