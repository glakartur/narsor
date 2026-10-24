//! The base-image contract, checked before any work (R22).
//!
//! A repository brings its own base image and Narsor only injects its
//! runtime (R20, R21), so nothing guarantees that the image can host a
//! runner. Every requirement is checked up front and *all* failures are
//! reported at once: a task whose image lacks `git` must fail before its
//! clone, not half-way through its first agent step, and the owner should
//! fix the image in one round rather than one requirement at a time.
//!
//! The runner never repairs anything it finds missing. Installing packages
//! or logging in at start-up is exactly what the remote model exists to
//! avoid (R22); the fix belongs in the image.
//!
//! "The start command can be overridden" is part of the contract too, but
//! needs no check here: a runner that is running proves it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The requirements a base image must meet.
///
/// A value rather than constants, so a test can describe an image that
/// lacks something without needing such an image.
#[derive(Debug, Clone)]
pub struct BaseImageContract {
    /// Supported operating systems (`std::env::consts::OS`).
    pub platforms: Vec<String>,
    /// Supported architectures (`std::env::consts::ARCH`).
    pub architectures: Vec<String>,
    /// The shell every script action and harness wrapper relies on.
    pub shell: PathBuf,
    /// Basic tools that must be on `PATH`.
    pub tools: Vec<String>,
    /// CA bundle files, any one of which is enough.
    pub ca_bundles: Vec<PathBuf>,
    /// Whether the runner must not run as root.
    pub require_non_root: bool,
    /// The `PATH` the tools are looked for on; `None` is this process's.
    pub path: Option<std::ffi::OsString>,
}

impl Default for BaseImageContract {
    fn default() -> Self {
        Self {
            platforms: vec!["linux".to_string()],
            architectures: vec!["x86_64".to_string(), "aarch64".to_string()],
            shell: PathBuf::from("/bin/sh"),
            tools: ["cat", "cp", "env", "grep", "ls", "mkdir", "mv", "rm", "sed"]
                .into_iter()
                .map(String::from)
                .collect(),
            ca_bundles: [
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/ca-bundle.pem",
                "/etc/ssl/cert.pem",
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect(),
            require_non_root: true,
            path: None,
        }
    }
}

/// One unmet requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// Short name of the requirement (`git`, `ca-certificates`…).
    pub requirement: &'static str,
    /// What was found instead.
    pub detail: String,
}

impl std::fmt::Display for ContractViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.requirement, self.detail)
    }
}

impl BaseImageContract {
    /// Checks the running environment against the contract, with `home` as
    /// the private HOME that must be writable. Returns every violation.
    pub fn check(&self, home: &Path) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut violation = |requirement, detail: String| {
            violations.push(ContractViolation {
                requirement,
                detail,
            })
        };

        let os = std::env::consts::OS;
        if !self.platforms.iter().any(|platform| platform == os) {
            violation(
                "platform",
                format!("{os} is not one of {}", self.platforms.join(", ")),
            );
        }
        let arch = std::env::consts::ARCH;
        if !self.architectures.iter().any(|known| known == arch) {
            violation(
                "architecture",
                format!("{arch} is not one of {}", self.architectures.join(", ")),
            );
        }

        if !is_executable(&self.shell) {
            violation(
                "shell",
                format!("{} is missing or not executable", self.shell.display()),
            );
        }

        let path = self
            .path
            .clone()
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let missing: Vec<&str> = self
            .tools
            .iter()
            .map(String::as_str)
            .filter(|tool| find_on_path(tool, &path).is_none())
            .collect();
        if !missing.is_empty() {
            violation(
                "basic tools",
                format!("not on PATH: {}", missing.join(", ")),
            );
        }

        match find_on_path("git", &path) {
            None => violation("git", "git is not on PATH".to_string()),
            Some(git) => match Command::new(&git).arg("--version").output() {
                Ok(output) if output.status.success() => {}
                Ok(output) => violation(
                    "git",
                    format!(
                        "`git --version` failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                ),
                Err(error) => violation("git", format!("{} cannot run: {error}", git.display())),
            },
        }

        let from_env = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
        let has_bundle = from_env
            .iter()
            .chain(self.ca_bundles.iter())
            .any(|bundle| fs::metadata(bundle).is_ok_and(|meta| meta.is_file() && meta.len() > 0));
        if !has_bundle {
            violation(
                "ca-certificates",
                format!(
                    "no CA bundle at SSL_CERT_FILE or any of {}",
                    self.ca_bundles
                        .iter()
                        .map(|bundle| bundle.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }

        // SAFETY: `geteuid` has no preconditions and cannot fail.
        if self.require_non_root && unsafe { libc::geteuid() } == 0 {
            violation(
                "non-root",
                "the runner runs as root; the image must run it as an unprivileged user"
                    .to_string(),
            );
        }

        if let Err(detail) = probe_writable(home) {
            violation("writable HOME", detail);
        }

        violations
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

pub(crate) fn find_on_path(tool: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable(candidate))
}

fn probe_writable(home: &Path) -> Result<(), String> {
    let probe = home.join(format!(".narsor-probe-{}", std::process::id()));
    fs::write(&probe, b"probe")
        .map_err(|error| format!("{} is not writable: {error}", home.display()))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("{} cannot be cleaned: {error}", home.display()))
}
