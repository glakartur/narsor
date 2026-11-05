//! The list of branches a repository has on its remote: the real `git
//! ls-remote` run against a bare repository on disk, the command it spawns,
//! and the route and client that serve it, with a double in place of git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use narsor_api_client::api::ClientError;
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::engine::Engine;
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::{ExecutionStore, StatePaths};
use narsor_engine::registry::{
    ls_remote_command, BranchListError, BranchLister, GitBranchLister, RepositoryEntry,
    RepositoryRegistry,
};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-branches-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A bare repository with the branches `main`, `develop` and `feature/x` and
/// the tag `v1`.
fn bare_repository(root: &Path) -> PathBuf {
    let work = root.join("work");
    let bare = root.join("origin.git");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "--initial-branch=main"]);
    git(&work, &["commit", "--allow-empty", "-m", "first"]);
    git(&work, &["branch", "develop"]);
    git(&work, &["branch", "feature/x"]);
    git(&work, &["tag", "v1"]);
    git(
        root,
        &[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

fn lister() -> GitBranchLister {
    GitBranchLister::new(Duration::from_secs(20))
}

#[test]
fn only_branches_are_listed_and_a_slash_stays_in_the_name() {
    let root = TempRoot::new();
    let bare = bare_repository(root.path());

    let mut branches = lister().list_branches(bare.to_str().unwrap()).unwrap();
    branches.sort();

    assert_eq!(branches, ["develop", "feature/x", "main"]);
}

#[test]
fn a_remote_that_cannot_be_reached_is_an_error_with_a_reason_not_an_empty_list() {
    let root = TempRoot::new();
    let missing = root.path().join("nowhere.git");

    let error = lister()
        .list_branches(missing.to_str().unwrap())
        .unwrap_err();

    let BranchListError::Failed(reason) = error else {
        panic!("expected a failure, got {error:?}");
    };
    assert!(!reason.trim().is_empty());
}

#[test]
fn credentials_in_a_remote_never_reach_the_error_text() {
    let root = TempRoot::new();
    // Git repeats the path it could not open in its own message.
    let missing = root.path().join("s3cret@nowhere.git");

    let error = lister()
        .list_branches(missing.to_str().unwrap())
        .unwrap_err();
    assert!(!error.to_string().contains("s3cret"), "{error}");

    for remote in [
        "https://user:s3cret@localhost/repo.git",
        "https://s3cret@localhost/repo.git",
        "ssh://user:s3cret@example.com/repo.git",
    ] {
        let error = lister().list_branches(remote).unwrap_err();
        assert!(!error.to_string().contains("s3cret"), "{remote}: {error}");
    }
}

#[test]
fn a_remote_beyond_the_allowed_kinds_is_refused_before_git_runs() {
    for remote in [
        "http://example.com/repo.git",
        "ssh://git@example.com/repo.git",
        "git@example.com:acme/repo.git",
        "git://example.com/repo.git",
        "file:///srv/repo.git",
        "https://localhost/repo.git",
        "https://127.0.0.1/repo.git",
        "https://10.1.2.3/repo.git",
        "https://[::1]/repo.git",
        "ext::sh -c id",
        "-oProxyCommand=id",
        "",
    ] {
        // A refusal is not what a failed git run looks like.
        assert!(
            matches!(
                lister().list_branches(remote),
                Err(BranchListError::Refused(_))
            ),
            "{remote:?} should be refused"
        );
    }
}

#[test]
fn the_command_carries_the_hardening_and_none_of_the_users_git_setup() {
    let command = ls_remote_command("https://example.com/acme/repo.git");

    assert_eq!(command.get_program(), "git");
    let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
    assert_eq!(
        args,
        [
            "ls-remote",
            "--heads",
            "--",
            "https://example.com/acme/repo.git"
        ]
    );

    let envs: std::collections::HashMap<String, Option<String>> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let set = |name: &str| envs.get(name).cloned().flatten();
    assert_eq!(set("GIT_TERMINAL_PROMPT").as_deref(), Some("0"));
    assert_eq!(set("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
    assert_eq!(set("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
    assert_eq!(set("GIT_CONFIG_COUNT").as_deref(), Some("1"));
    assert_eq!(
        set("GIT_CONFIG_KEY_0").as_deref(),
        Some("http.followRedirects")
    );
    assert_eq!(set("GIT_CONFIG_VALUE_0").as_deref(), Some("false"));
    assert_eq!(set("GIT_ALLOW_PROTOCOL").as_deref(), Some("https"));
    for name in [
        "SSH_AUTH_SOCK",
        "SSH_ASKPASS",
        "GIT_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "XDG_CONFIG_HOME",
    ] {
        assert_eq!(
            envs.get(name),
            Some(&None),
            "{name} must be taken from the environment git inherits"
        );
    }
}

#[test]
fn a_local_path_is_the_only_remote_that_may_use_the_file_transport() {
    let command = ls_remote_command("/srv/git/repo.git");
    let allowed = command
        .get_envs()
        .find(|(name, _)| *name == "GIT_ALLOW_PROTOCOL")
        .and_then(|(_, value)| value)
        .map(|value| value.to_string_lossy().into_owned());
    assert_eq!(allowed.as_deref(), Some("file"));
}

struct Double {
    answer: Result<Vec<String>, BranchListError>,
    asked: Mutex<Vec<String>>,
}

impl Double {
    fn answering(answer: Result<Vec<String>, BranchListError>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl BranchLister for Double {
    fn list_branches(&self, remote: &str) -> Result<Vec<String>, BranchListError> {
        self.asked.lock().unwrap().push(remote.to_string());
        match &self.answer {
            Ok(branches) => Ok(branches.clone()),
            Err(BranchListError::TimedOut { limit }) => {
                Err(BranchListError::TimedOut { limit: *limit })
            }
            Err(BranchListError::Failed(reason)) => Err(BranchListError::Failed(reason.clone())),
            Err(BranchListError::Refused(reason)) => Err(BranchListError::Refused(reason.clone())),
        }
    }
}

async fn serve(root: &TempRoot, lister: Arc<Double>) -> ApiHandle {
    let catalog = root.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let engine = Engine::new(ExecutionStore::new(root.path().join("store")));
    // The owner routes of the control plane exist only with a control plane.
    let state = root.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let control = ProtocolService::new(
        ControlPlane::new(StatePaths::new(&state)),
        crate::environment_support::seeded_resolver(&state),
    );
    let registry = RepositoryRegistry::new(&catalog);
    registry
        .add(RepositoryEntry::new(
            "acme-api",
            "https://example.com/acme/api.git",
        ))
        .unwrap();
    start_with_services(
        Arc::new(engine),
        catalog,
        root.path().join("work"),
        ApiServices {
            control: Some(Arc::new(control)),
            branch_lister: Some(lister),
            ..ApiServices::default()
        },
    )
    .await
    .expect("start the server")
}

#[tokio::test]
async fn the_client_gets_the_branch_names_of_a_registered_repository() {
    let root = TempRoot::new();
    let double = Double::answering(Ok(vec!["main".into(), "feature/x".into()]));
    let server = serve(&root, Arc::clone(&double)).await;

    let response = server
        .client()
        .control_repository_branches("acme-api")
        .await
        .unwrap();

    assert_eq!(response.branches, ["main", "feature/x"]);
    assert_eq!(
        *double.asked.lock().unwrap(),
        ["https://example.com/acme/api.git"],
        "the remote comes from the registry, not from the request"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_repository_is_not_found_and_git_is_not_asked() {
    let root = TempRoot::new();
    let double = Double::answering(Ok(vec![]));
    let server = serve(&root, Arc::clone(&double)).await;

    let error = server
        .client()
        .control_repository_branches("nope")
        .await
        .unwrap_err();

    assert!(matches!(error, ClientError::NotFound), "{error:?}");
    assert!(double.asked.lock().unwrap().is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn a_failed_listing_reaches_the_client_as_an_error_with_the_reason() {
    let root = TempRoot::new();
    let server = serve(
        &root,
        Double::answering(Err(BranchListError::Failed(
            "fatal: could not read\u{1b}[31m from remote".into(),
        ))),
    )
    .await;

    let error = server
        .client()
        .control_repository_branches("acme-api")
        .await
        .unwrap_err();

    let ClientError::Degraded { message, .. } = error else {
        panic!("expected an error with a reason, got {error:?}");
    };
    assert!(message.contains("could not read"), "{message}");
    assert!(!message.contains('\u{1b}'), "{message:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn a_listing_that_runs_out_of_time_names_the_limit() {
    let root = TempRoot::new();
    let server = serve(
        &root,
        Double::answering(Err(BranchListError::TimedOut {
            limit: Duration::from_secs(20),
        })),
    )
    .await;

    let error = server
        .client()
        .control_repository_branches("acme-api")
        .await
        .unwrap_err();

    let ClientError::Degraded { message, .. } = error else {
        panic!("expected an error with a reason, got {error:?}");
    };
    assert!(message.contains("20 s"), "{message}");
    server.shutdown().await;
}

#[tokio::test]
async fn a_remote_the_server_refuses_is_a_request_error_with_the_reason() {
    let root = TempRoot::new();
    let server = serve(
        &root,
        Double::answering(Err(BranchListError::Refused("not an https URL".into()))),
    )
    .await;

    let error = server
        .client()
        .control_repository_branches("acme-api")
        .await
        .unwrap_err();

    let ClientError::Request { message, .. } = error else {
        panic!("expected a request error, got {error:?}");
    };
    assert!(message.contains("not an https URL"), "{message}");
    server.shutdown().await;
}
