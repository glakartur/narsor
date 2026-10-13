//! Which configuration file wins, and why the losers are named rather than
//! forgotten.
//!
//! Three places are tried in order — the path given with `--config`, the
//! user's config directory, and `/etc/narsor/` — and the first one that holds
//! a file wins outright. Nothing is merged: R3 asks for a file, not for a
//! layered document, and merging would make "where did this value come from"
//! unanswerable in exactly the situation the answer is wanted.
//!
//! `/etc/narsor/` is last because it is the mount point a ConfigMap is
//! expected to land on: a container's operator overrides it from the command
//! line, and a developer's laptop overrides it from `~/.config`.
//!
//! # Why a broken candidate stops the start
//!
//! [`WorkflowResolver`](crate::workflows::WorkflowResolver) skips a candidate
//! it cannot read and moves on. This module does the opposite on purpose: a
//! candidate that exists but is not a readable file produces an error naming
//! the path and the reason (D-G).
//!
//! The failures this catches are the ones a ConfigMap produces — a volume
//! mounted over the path of a file, the window during an atomic swap when the
//! symlink dangles, a wrong `defaultMode` denying the read. Every one of them
//! would otherwise present as "no configuration at all", and silently running
//! on built-in values while a file sits right there is the worst outcome this
//! whole feature exists to prevent.
//!
//! A candidate that is simply *absent* is still a normal, silent state (R4).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use std::env;

/// The directory a system-wide configuration file is read from.
///
/// The intended mount point of a ConfigMap.
pub const SYSTEM_CONFIG_DIR: &str = "/etc/narsor";

/// The stem of the configuration file, without an extension.
pub const CONFIG_STEM: &str = "config";

/// The extensions tried in each directory, in order.
///
/// Both spellings, because a `.yml` file silently ignored is the same failure
/// this feature exists to prevent (D-I).
pub const EXTENSIONS: [&str; 2] = ["yaml", "yml"];

/// Which step of the cascade a candidate belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// The path the operator named with `--config`.
    Flag,
    /// The user's own config directory.
    User,
    /// `/etc/narsor/`.
    System,
}

/// One place the cascade looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Which step it belongs to.
    pub kind: CandidateKind,
    /// The path itself.
    pub path: PathBuf,
}

/// What the cascade found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolution {
    /// The file that won, if any. `None` is a normal, silent state (R4).
    pub winner: Option<Candidate>,
    /// Files that exist but lost, in the order they were tried.
    ///
    /// Never dropped: an operator has to be able to see that the file feeding
    /// this run is not the one they edited.
    pub shadowed: Vec<Candidate>,
}

impl Resolution {
    /// The path of the winning file, if there is one.
    pub fn path(&self) -> Option<&Path> {
        self.winner
            .as_ref()
            .map(|candidate| candidate.path.as_path())
    }
}

/// Everything that can stop the cascade.
///
/// Each variant names the path, because a message that does not is useless in
/// the container where these failures happen.
#[derive(Debug, thiserror::Error)]
pub enum CascadeError {
    /// `--config` pointed at something that is not there.
    ///
    /// A usage error rather than a configuration error: the operator named
    /// this file, so falling through to the next step of the cascade would be
    /// obeying something they did not ask for.
    #[error("Configuration file '{path}' does not exist.")]
    MissingNamedFile {
        /// The path as it was given.
        path: PathBuf,
    },

    /// Something is mounted over the path, most often a directory.
    #[error("Configuration path '{path}' is a directory, not a file.")]
    NotAFile {
        /// The offending path.
        path: PathBuf,
    },

    /// The entry is a symlink whose target is gone.
    #[error("Configuration file '{path}' is a symbolic link with no target.")]
    BrokenLink {
        /// The link itself.
        path: PathBuf,
    },

    /// The entry is there but cannot be opened.
    #[error("Configuration file '{path}' cannot be read: {source}")]
    Unreadable {
        /// The file that refused.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
}

impl CascadeError {
    /// The path the message is about.
    pub fn path(&self) -> &Path {
        match self {
            CascadeError::MissingNamedFile { path }
            | CascadeError::NotAFile { path }
            | CascadeError::BrokenLink { path }
            | CascadeError::Unreadable { path, .. } => path,
        }
    }

    /// Whether this is the operator naming a file that is not there.
    ///
    /// The one configuration failure that is a *usage* error, and therefore
    /// the one `config show` does not survive either (D-D).
    pub fn is_usage(&self) -> bool {
        matches!(self, CascadeError::MissingNamedFile { .. })
    }
}

/// The three places a configuration file is looked for.
///
/// The environment is read once, when this is constructed, so that everything
/// downstream works off a fixed set of paths.
#[derive(Debug, Clone)]
pub struct ConfigCascade {
    named: Option<PathBuf>,
    user_dir: Option<PathBuf>,
    system_dir: Option<PathBuf>,
}

impl ConfigCascade {
    /// A cascade with the user directory taken from the environment and the
    /// system directory at its usual place.
    pub fn new(named: Option<PathBuf>) -> Self {
        Self {
            named,
            user_dir: user_config_dir(),
            system_dir: Some(PathBuf::from(SYSTEM_CONFIG_DIR)),
        }
    }

    /// The same, with both directories named explicitly.
    ///
    /// `None` disables that step entirely, which is how a test guarantees it
    /// never reads the developer's own `~/.config` or the machine's `/etc`.
    pub fn with_dirs(
        named: Option<PathBuf>,
        user_dir: Option<PathBuf>,
        system_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            named,
            user_dir,
            system_dir,
        }
    }

    /// Every place that will be looked at, in order.
    pub fn candidates(&self) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        if let Some(path) = &self.named {
            candidates.push(Candidate {
                kind: CandidateKind::Flag,
                path: path.clone(),
            });
        }
        for (kind, directory) in [
            (CandidateKind::User, &self.user_dir),
            (CandidateKind::System, &self.system_dir),
        ] {
            let Some(directory) = directory else { continue };
            for extension in EXTENSIONS {
                candidates.push(Candidate {
                    kind,
                    path: directory.join(format!("{CONFIG_STEM}.{extension}")),
                });
            }
        }
        candidates
    }

    /// Walks the cascade and reports the winner, or the absence of one.
    ///
    /// Nothing is created: resolution never so much as makes a directory, so
    /// it works on a read-only filesystem and without a home (R9).
    pub fn resolve(&self) -> Result<Resolution, CascadeError> {
        let candidates = self.candidates();
        let mut winner: Option<Candidate> = None;
        let mut shadowed = Vec::new();

        for candidate in candidates {
            // Past the winner, a candidate only has to be known to exist: its
            // readability is never used, and checking it would open and
            // canonicalize a file nothing will read.
            let found = match &winner {
                Some(_) => presence_of(&candidate.path),
                None => inspect(&candidate.path),
            };
            match found {
                Presence::Absent => {
                    // A file that is not there is not a problem (R4).
                    if candidate.kind == CandidateKind::Flag {
                        return Err(CascadeError::MissingNamedFile {
                            path: candidate.path,
                        });
                    }
                }
                Presence::Broken(error) => {
                    // A candidate is only inspected closely while there is no
                    // winner, so a broken file below the winner cannot stop a
                    // run that was never going to read it.
                    return Err(error);
                }
                Presence::Readable => {
                    // Absolutized, the way `WorkflowOrigin::File` already is: a
                    // relative `XDG_CONFIG_HOME` is accepted by the code around
                    // it, and printing "config.yaml" as the answer to "which
                    // file did this come from" would only half answer it (R8).
                    match winner {
                        None => {
                            winner = Some(Candidate {
                                kind: candidate.kind,
                                path: absolutize(&candidate.path),
                            })
                        }
                        Some(_) => shadowed.push(candidate),
                    }
                }
            }
        }

        Ok(Resolution { winner, shadowed })
    }
}

fn user_config_dir() -> Option<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(config_home).join("narsor"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".config/narsor"))
}

/// What one candidate path turned out to be.
enum Presence {
    /// Nothing is there.
    Absent,
    /// Something is there and can be read.
    Readable,
    /// Something is there and cannot be read, with the reason.
    Broken(CascadeError),
}

/// The path made absolute, without following symbolic links.
///
/// Deliberately not `canonicalize`, which
/// [`WorkflowOrigin::File`](crate::workflows::WorkflowOrigin) uses: a
/// ConfigMap mounts `/etc/narsor/config.yaml` as a link to `..data/…`, itself
/// a link to a timestamped snapshot directory that is replaced on the next
/// update. Resolving that chain would report a path which is real for a few
/// seconds and is not the one the operator mounted or would edit — the
/// opposite of what naming the source is for (R8).
fn absolutize(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether anything is there at all, without asking whether it can be read.
///
/// What a candidate below the winner needs: it is reported as shadowed, and
/// nothing will ever open it.
fn presence_of(path: &Path) -> Presence {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Presence::Readable,
        Err(error) if error.kind() == ErrorKind::NotFound => Presence::Absent,
        Err(_) => Presence::Absent,
    }
}

/// Classifies one candidate path.
///
/// `symlink_metadata` first, so that a dangling link is distinguished from an
/// absent file: `metadata` follows the link and reports both as `NotFound`,
/// and the two want opposite answers — one is silence, the other is a refusal
/// to start.
fn inspect(path: &Path) -> Presence {
    let link = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Presence::Absent,
        Err(source) => {
            return Presence::Broken(CascadeError::Unreadable {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let metadata = if link.file_type().is_symlink() {
        match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Presence::Broken(CascadeError::BrokenLink {
                    path: path.to_path_buf(),
                })
            }
            Err(source) => {
                return Presence::Broken(CascadeError::Unreadable {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
    } else {
        link
    };

    if metadata.is_dir() {
        return Presence::Broken(CascadeError::NotAFile {
            path: path.to_path_buf(),
        });
    }

    // Opened rather than merely stat'ed: a mode that denies the read is one of
    // the ways a ConfigMap arrives broken, and it is invisible to `metadata`.
    match std::fs::File::open(path) {
        Ok(_) => Presence::Readable,
        Err(source) => Presence::Broken(CascadeError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}
