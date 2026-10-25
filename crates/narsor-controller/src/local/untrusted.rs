//! What `narsor-controller local` refuses from a control plane beyond this
//! machine (R17, KTD14).
//!
//! Such a server is not trusted with this computer. It names tasks, and a
//! task's launch spec reaches the runner as written, so before anything of
//! a task is created the spec is checked:
//!
//! * a host directory to copy as the workspace must lie inside the
//!   controller's storage once symlinks and `..` are resolved — the server
//!   picks no directory of the user's;
//! * a Git workspace is cloned over `https://` only — never a path or a
//!   `file://`, `ssh://` or `git@host:` remote, which would clone a
//!   repository of the user's disk or one their SSH keys open; the runner
//!   is also started so that the clone uses none of the user's own git
//!   configuration and credentials, only the pinned one; nor may it name
//!   this machine or an address of a private network, which the server
//!   could probe through git's errors;
//! * a secret goes only into an environment variable a harness or tool the
//!   runner supports reads ([`ALLOWED_SECRET_ENVS`]), or one the user allowed
//!   with `--allow-env` — never `PATH`, `LD_PRELOAD` or the like;
//! * a secret file must be a relative path without `..`, inside the private
//!   HOME (the runner refuses the others too).
//!
//! The runner program is fixed (`narsor-runner` beside the controller) and
//! the spec has no unknown fields (the runner reads it with
//! `deny_unknown_fields`). None of this keeps a task's own code — what a
//! harness runs — from reading the user's files: a task runs as the user,
//! without a container.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use narsor_engine::domain::{GitIdentity, SecretDelivery, TaskLaunchSpec, TaskWorkspace};

/// The variables a secret may be delivered as without `--allow-env`: the
/// credentials of the harnesses and tools the runner supports.
pub const ALLOWED_SECRET_ENVS: [&str; 6] = [
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
];

/// The checks for one controller's storage and allowed variables.
#[derive(Debug, Clone)]
pub struct UntrustedServer {
    storage: PathBuf,
    allowed_envs: BTreeSet<String>,
}

impl UntrustedServer {
    /// Checks against `storage` (which must exist) and the built-in
    /// variables plus `extra_envs`.
    pub fn new(storage: &Path, extra_envs: &[String]) -> std::io::Result<Self> {
        Ok(Self {
            storage: storage.canonicalize()?,
            allowed_envs: ALLOWED_SECRET_ENVS
                .iter()
                .map(|name| name.to_string())
                .chain(extra_envs.iter().cloned())
                .collect(),
        })
    }

    /// Why `spec` is refused, if it is.
    pub fn check(&self, spec: &TaskLaunchSpec) -> Result<(), String> {
        match &spec.workspace {
            TaskWorkspace::None => {}
            TaskWorkspace::Local { path } => self.check_directory(path)?,
            TaskWorkspace::Git(identity) => check_remote(identity)?,
        }
        for secret in &spec.environment.secrets {
            match &secret.delivery {
                SecretDelivery::Env(name) if !self.allowed_envs.contains(name) => {
                    return Err(format!(
                        "secret '{}' would be delivered as environment variable '{name}', which \
                         this controller does not allow; allow it with --allow-env {name}",
                        secret.name
                    ));
                }
                SecretDelivery::Env(_) => {}
                SecretDelivery::File(_) => {
                    if let Some(problem) = secret.delivery.problem() {
                        return Err(format!("secret '{}': {problem}", secret.name));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_directory(&self, path: &str) -> Result<(), String> {
        let refuse = |reason: &str| {
            Err(format!(
                "the local workspace '{path}' {reason}; a control plane beyond this machine may \
                 only name directories inside {}",
                self.storage.display()
            ))
        };
        let Ok(canonical) = Path::new(path).canonicalize() else {
            return refuse("does not exist");
        };
        if !canonical.starts_with(&self.storage) {
            return refuse("lies outside the controller's storage");
        }
        Ok(())
    }
}

/// Refuses a Git remote other than an `https://` URL naming a host beyond
/// this machine and its private network.
fn check_remote(identity: &GitIdentity) -> Result<(), String> {
    let remote = &identity.remote_url;
    let not_https = || {
        format!(
            "the Git workspace '{}' is not an https:// URL; a control plane beyond this machine \
             may only have repositories cloned over https://",
            without_credentials(remote)
        )
    };
    // Git reads the text, not a parser's view of it: to git `https:host/x`
    // is an SSH remote, where a URL parser sees `https://host/x`.
    let plain = !remote
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\');
    if !plain || !remote.starts_with("https://") {
        return Err(not_https());
    }
    let url = reqwest::Url::parse(remote).map_err(|_| not_https())?;
    match url.host_str() {
        Some(host) if is_internal(host) => Err(format!(
            "the Git workspace '{}' names {host}, this machine or its private network; a \
             control plane beyond this machine may only have repositories cloned from beyond \
             it",
            without_credentials(remote)
        )),
        Some(_) => Ok(()),
        None => Err(not_https()),
    }
}

/// Whether `host` is this machine or an address of a private network: a
/// clone of it would let the server probe them through git's errors. A name
/// is not resolved; only `localhost` is known to be this machine.
fn is_internal(host: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr};

    let internal_v4 = |ip: &Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            // Shared address space (RFC 6598), carrier-grade NAT.
            || (a == 100 && (64..128).contains(&b))
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => internal_v4(&ip),
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                // Unique local (fc00::/7) and link-local (fe80::/10).
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(|ip| internal_v4(&ip))
        }
        Err(_) => {
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    }
}

/// `remote` as a refusal may show it: without a user name or password.
fn without_credentials(remote: &str) -> String {
    if let Ok(mut url) = reqwest::Url::parse(remote) {
        if !url.username().is_empty() || url.password().is_some() {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            return url.to_string();
        }
    }
    // What a parser does not see as credentials may still be some.
    match remote.rsplit_once('@') {
        Some((_, host)) => format!("...@{host}"),
        None => remote.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use narsor_engine::domain::{BackendId, ResolvedEnvironment, ResourceProfile, SecretReference};

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "narsor-untrusted-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(dir.join("storage")).unwrap();
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn spec(workspace: TaskWorkspace, secrets: Vec<SecretDelivery>) -> TaskLaunchSpec {
        let mut environment = ResolvedEnvironment::harness_only("claude-code");
        environment.secrets = secrets
            .into_iter()
            .enumerate()
            .map(|(index, delivery)| SecretReference {
                backend: BackendId::parse("laptop").unwrap(),
                name: format!("secret-{index}"),
                tool: "gh".to_string(),
                key: "key".to_string(),
                delivery,
            })
            .collect();
        TaskLaunchSpec {
            version: 1,
            workflow_id: "wf".to_string(),
            workflow_version: 1,
            inputs: serde_json::json!({}),
            workspace,
            environment: environment.sealed(),
            resources: ResourceProfile::new(1000, 2048),
            definition: None,
            ticket: None,
            title: None,
            digest: String::new(),
        }
    }

    fn server(dir: &Dir, extra: &[&str]) -> UntrustedServer {
        let extra: Vec<String> = extra.iter().map(|name| name.to_string()).collect();
        UntrustedServer::new(&dir.0.join("storage"), &extra).unwrap()
    }

    #[test]
    fn a_workspace_outside_the_storage_is_refused() {
        let dir = Dir::new();
        let server = server(&dir, &[]);
        let inside = dir.0.join("storage/project");
        std::fs::create_dir_all(&inside).unwrap();
        std::os::unix::fs::symlink("/etc", dir.0.join("storage/etc")).unwrap();
        assert!(server
            .check(&spec(
                TaskWorkspace::Local {
                    path: inside.display().to_string()
                },
                vec![]
            ))
            .is_ok());
        for path in [
            dir.0.join("storage/../outside").display().to_string(),
            dir.0.join("storage/project/../../").display().to_string(),
            dir.0.join("storage/etc").display().to_string(),
            "/etc".to_string(),
            "relative".to_string(),
        ] {
            let error = server
                .check(&spec(TaskWorkspace::Local { path: path.clone() }, vec![]))
                .expect_err(&path);
            assert!(
                error.contains("storage") || error.contains("exist"),
                "{error}"
            );
        }
        assert!(server.check(&spec(TaskWorkspace::None, vec![])).is_ok());
    }

    fn git(remote_url: &str) -> TaskWorkspace {
        TaskWorkspace::Git(GitIdentity {
            repository: None,
            remote_url: remote_url.to_string(),
            commit: String::new(),
            git_ref: None,
            ref_origin: None,
            credential: None,
        })
    }

    #[test]
    fn a_git_workspace_is_cloned_over_https_only() {
        let dir = Dir::new();
        let server = server(&dir, &[]);
        assert!(server
            .check(&spec(git("https://github.com/acme/app.git"), vec![]))
            .is_ok());
        for remote in [
            "/home/me/secret-repo",
            "../secret-repo",
            "file:///home/me/secret-repo",
            "ssh://git@github.com/acme/private.git",
            "git@github.com:acme/private.git",
            "http://github.com/acme/app.git",
            "ext::sh -c touch% /tmp/pwned",
            "https:github.com/acme/app.git",
            "https:/github.com/acme/app.git",
            "HTTPS://github.com/acme/app.git",
            " https://github.com/acme/app.git",
            "https://github.com\\@evil.example/app.git",
        ] {
            let error = server.check(&spec(git(remote), vec![])).expect_err(remote);
            assert!(error.contains("https://"), "{error}");
        }
    }

    #[test]
    fn a_git_workspace_on_this_machine_or_its_network_is_refused() {
        let dir = Dir::new();
        let server = server(&dir, &[]);
        for remote in [
            "https://localhost/acme/app.git",
            "https://git.localhost./acme/app.git",
            "https://127.0.0.1:8443/acme/app.git",
            "https://2130706433/acme/app.git",
            "https://10.1.2.3/acme/app.git",
            "https://172.16.0.1/acme/app.git",
            "https://192.168.1.10/acme/app.git",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/acme/app.git",
            "https://0.0.0.0/acme/app.git",
            "https://[::1]/acme/app.git",
            "https://[fd00::1]/acme/app.git",
            "https://[fe80::1]/acme/app.git",
            "https://[::ffff:10.0.0.1]/acme/app.git",
        ] {
            let error = server.check(&spec(git(remote), vec![])).expect_err(remote);
            assert!(error.contains("private network"), "{error}");
        }
        assert!(server
            .check(&spec(git("https://140.82.112.3/acme/app.git"), vec![]))
            .is_ok());
    }

    #[test]
    fn a_refusal_does_not_show_the_remotes_credentials() {
        let dir = Dir::new();
        let server = server(&dir, &[]);
        for remote in [
            "http://user:s3cret@github.com/acme/app.git",
            "https://user:s3cret@10.0.0.1/acme/app.git",
            "https:user:s3cret@github.com/acme/app.git",
            "user:s3cret@github.com:acme/app.git",
        ] {
            let error = server.check(&spec(git(remote), vec![])).expect_err(remote);
            assert!(!error.contains("s3cret"), "{error}");
        }
    }

    #[test]
    fn a_secret_goes_only_into_an_allowed_variable() {
        let dir = Dir::new();
        for name in ["LD_PRELOAD", "PATH", "NARSOR_X"] {
            let error = server(&dir, &[])
                .check(&spec(
                    TaskWorkspace::None,
                    vec![SecretDelivery::Env(name.into())],
                ))
                .expect_err(name);
            assert!(error.contains("--allow-env"), "{error}");
        }
        for name in ALLOWED_SECRET_ENVS {
            assert!(server(&dir, &[])
                .check(&spec(
                    TaskWorkspace::None,
                    vec![SecretDelivery::Env(name.into())]
                ))
                .is_ok());
        }
        assert!(server(&dir, &["MY_TOOL_TOKEN"])
            .check(&spec(
                TaskWorkspace::None,
                vec![SecretDelivery::Env("MY_TOOL_TOKEN".into())]
            ))
            .is_ok());
    }

    #[test]
    fn a_secret_file_stays_inside_the_private_home() {
        let dir = Dir::new();
        for path in [
            "../../.ssh/authorized_keys",
            "/home/me/.ssh/authorized_keys",
        ] {
            assert!(
                server(&dir, &[])
                    .check(&spec(
                        TaskWorkspace::None,
                        vec![SecretDelivery::File(path.into())]
                    ))
                    .is_err(),
                "{path}"
            );
        }
        assert!(server(&dir, &[])
            .check(&spec(
                TaskWorkspace::None,
                vec![SecretDelivery::File(".config/gh/hosts.yml".into())]
            ))
            .is_ok());
    }

    #[test]
    fn a_launch_spec_with_an_unknown_field_is_refused() {
        use narsor_protocol::launch::LaunchSpecEnvelope;
        let mut value = serde_json::to_value(spec(TaskWorkspace::None, vec![])).unwrap();
        assert!(serde_json::from_value::<LaunchSpecEnvelope>(value.clone()).is_ok());
        value
            .as_object_mut()
            .unwrap()
            .insert("program".into(), serde_json::json!("/bin/sh"));
        assert!(serde_json::from_value::<LaunchSpecEnvelope>(value).is_err());
    }
}
