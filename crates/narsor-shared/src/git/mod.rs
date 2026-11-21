//! Git rules shared by the processes that run `git` for someone else: the
//! remotes an untrusted caller may name and the parts of the user's own git
//! setup such a clone must not see.

pub mod remote_policy;

pub use remote_policy::{check_https_remote, without_credentials, RemoteWording};

/// The variables through which git would reach the user's own credentials
/// or configuration, none of which a clone pinned to its own credential
/// sees.
pub const USER_GIT_ACCESS_ENVS: [&str; 9] = [
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "XDG_CONFIG_HOME",
];
