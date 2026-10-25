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

use narsor_engine::domain::{SecretDelivery, TaskLaunchSpec, TaskWorkspace};

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
        if let TaskWorkspace::Local { path } = &spec.workspace {
            self.check_directory(path)?;
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
