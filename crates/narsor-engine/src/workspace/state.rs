//! What a task's git workspace shows about itself: the branch the work is on
//! and how many lines it changed against its base branch.
//!
//! The functions only read. A read that fails gives `None`, and the caller
//! keeps what it knew before. Untracked files are not counted: `git diff`
//! does not see them, and counting them would mean changing the index.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::provisioner::{git_command, GitAccess};
use crate::domain::{GitIdentity, TaskRepositorySource, TaskWorkspace, WorkspaceHeader};
use crate::tasks::repository_name;

/// Lines added and removed against a base branch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangedLines {
    pub added: u64,
    pub removed: u64,
}

/// How long one reading may take in all. A git that is still running then is
/// killed, so a slow repository does not pile up readings.
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// One reading of the git state of a workspace. A part that could not be
/// read is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitReading {
    /// The branch `HEAD` points at; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The branch the changes are counted against. Read from the remote
    /// `HEAD` when the task did not name one.
    pub base_branch: Option<String>,
    /// The changes against the base branch.
    pub changes: Option<ChangedLines>,
}

/// The header of each repository of the task as the launch spec describes it:
/// the repository, its start branch (which is also the base branch) and
/// nothing read from git. Local folders show their path only.
pub fn launch_headers(workspace: &TaskWorkspace) -> Vec<WorkspaceHeader> {
    workspace
        .repositories
        .iter()
        .map(|repository| match &repository.source {
            TaskRepositorySource::Local { path } => WorkspaceHeader {
                name: repository.name.clone(),
                repo: path.clone(),
                ..WorkspaceHeader::default()
            },
            TaskRepositorySource::Git(identity) => WorkspaceHeader {
                name: repository.name.clone(),
                repo: git_repository_name(identity),
                branch: repository.branch.clone(),
                base_branch: repository.branch.clone(),
                ..WorkspaceHeader::default()
            },
        })
        .collect()
}

fn git_repository_name(identity: &GitIdentity) -> String {
    identity
        .repository
        .clone()
        .unwrap_or_else(|| repository_name(&identity.remote_url))
}

/// Reads the branch and the changed lines of the repository in `dir`.
/// `base_branch` is the branch the task started from, when it is known.
pub fn read(dir: &Path, base_branch: Option<&str>) -> GitReading {
    read_within(dir, base_branch, READ_TIMEOUT)
}

/// [`read`] with its own limit. When the limit passes, the git process that
/// runs is killed and the parts not yet read are `None`.
pub fn read_within(dir: &Path, base_branch: Option<&str>, limit: Duration) -> GitReading {
    let git = Git {
        deadline: Instant::now() + limit,
    };
    let dir = dir.to_string_lossy();
    let base_branch = base_branch
        .map(str::to_owned)
        .or_else(|| remote_head_branch(&git, &dir));
    let changes = base_branch
        .as_deref()
        .and_then(|base| changed_lines(&git, &dir, base));
    GitReading {
        branch: current_branch(&git, &dir),
        base_branch,
        changes,
    }
}

/// The branch `HEAD` points at; `None` for a detached `HEAD` and for a
/// directory that is not a repository.
fn current_branch(git: &Git, dir: &str) -> Option<String> {
    git.stdout(&["-C", dir, "symbolic-ref", "--quiet", "--short", "HEAD"])
        .filter(|branch| !branch.is_empty())
}

/// The branch the remote's `HEAD` points at, as the clone recorded it.
fn remote_head_branch(git: &Git, dir: &str) -> Option<String> {
    let head = git.stdout(&[
        "-C",
        dir,
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ])?;
    head.strip_prefix("origin/")
        .filter(|branch| !branch.is_empty())
        .map(str::to_owned)
}

/// Lines changed from the common ancestor of the base branch and `HEAD` to
/// the working tree. A fresh clone has only the remote base branch, so
/// `origin/<base>` wins over a local `<base>`.
fn changed_lines(git: &Git, dir: &str, base_branch: &str) -> Option<ChangedLines> {
    let base = base_ref(git, dir, base_branch)?;
    let ancestor = git.stdout(&["-C", dir, "merge-base", &base, "HEAD"])?;
    // `--no-optional-locks` keeps the read from refreshing the index.
    let numstat = git.stdout(&[
        "--no-optional-locks",
        "-C",
        dir,
        "diff",
        "--numstat",
        "--no-renames",
        &ancestor,
    ])?;
    Some(sum_numstat(&numstat))
}

fn base_ref(git: &Git, dir: &str, base_branch: &str) -> Option<String> {
    [
        format!("refs/remotes/origin/{base_branch}"),
        format!("refs/heads/{base_branch}"),
    ]
    .into_iter()
    .find(|candidate| {
        git.stdout(&["-C", dir, "rev-parse", "--verify", "--quiet", candidate])
            .is_some()
    })
}

/// Runs read-only git commands that all share one deadline.
struct Git {
    deadline: Instant,
}

impl Git {
    /// The trimmed stdout of a git that succeeded; `None` when git failed,
    /// could not start, or was killed at the deadline.
    fn stdout(&self, args: &[&str]) -> Option<String> {
        if Instant::now() >= self.deadline {
            return None;
        }
        let mut child = git_command(args, &GitAccess::User)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Drained on its own thread: a full pipe would stop git before it exits.
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            bytes
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < self.deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let bytes = reader.join().ok()?;
        status
            .filter(|status| status.success())
            .map(|_| String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

/// Adds up the lines of `git diff --numstat`. A binary file shows `-` and
/// counts as no lines.
fn sum_numstat(numstat: &str) -> ChangedLines {
    numstat
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let added = columns.next()?;
            let removed = columns.next()?;
            Some((added.parse().unwrap_or(0), removed.parse().unwrap_or(0)))
        })
        .fold(ChangedLines::default(), |total, (added, removed)| {
            ChangedLines {
                added: total.added + added,
                removed: total.removed + removed,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskRepository;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("narsor-workspace-state-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-q", "-m", name]);
    }

    fn origin_and_clone(scratch: &Scratch) -> PathBuf {
        let origin = scratch.0.join("origin");
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        commit_file(&origin, "a.txt", "1\n2\n3\n");
        let clone = scratch.0.join("clone");
        git(&scratch.0, &["clone", "-q", "origin", "clone"]);
        clone
    }

    #[test]
    fn a_clone_on_its_base_branch_is_on_it_with_no_changes() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);

        let reading = read(&clone, Some("main"));

        assert_eq!(reading.branch.as_deref(), Some("main"));
        assert_eq!(reading.base_branch.as_deref(), Some("main"));
        assert_eq!(reading.changes, Some(ChangedLines::default()));
    }

    #[test]
    fn a_new_branch_counts_its_lines_from_the_remote_base() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        git(&clone, &["checkout", "-q", "-b", "feat/x"]);
        commit_file(&clone, "b.txt", "x\ny\n");
        commit_file(&clone, "a.txt", "1\n3\n4\n5\n");

        let reading = read(&clone, Some("main"));

        assert_eq!(reading.branch.as_deref(), Some("feat/x"));
        assert_eq!(reading.base_branch.as_deref(), Some("main"));
        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 4,
                removed: 1
            })
        );
    }

    #[test]
    fn the_changes_are_measured_from_the_merge_base_not_from_the_base_tip() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        git(&clone, &["checkout", "-q", "-b", "feat/x"]);
        commit_file(&clone, "b.txt", "x\n");
        let origin = scratch.0.join("origin");
        commit_file(&origin, "c.txt", "1\n2\n3\n4\n5\n6\n");
        git(&clone, &["fetch", "-q", "origin"]);

        let reading = read(&clone, Some("main"));

        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 1,
                removed: 0
            })
        );
    }

    #[test]
    fn an_uncommitted_change_to_a_tracked_file_counts_and_a_new_file_does_not() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        fs::write(clone.join("a.txt"), "1\n2\n3\n4\n").unwrap();
        fs::write(clone.join("untracked.txt"), "a\nb\nc\n").unwrap();

        let reading = read(&clone, Some("main"));

        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 1,
                removed: 0
            })
        );
    }

    #[test]
    fn a_detached_head_has_no_branch_and_still_counts_changes() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        git(&clone, &["checkout", "-q", "--detach"]);
        fs::write(clone.join("a.txt"), "1\n").unwrap();

        let reading = read(&clone, Some("main"));

        assert_eq!(reading.branch, None);
        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 0,
                removed: 2
            })
        );
    }

    #[test]
    fn a_base_branch_that_exists_only_locally_is_used() {
        let scratch = Scratch::new();
        let repository = scratch.0.join("plain");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-q"]);
        commit_file(&repository, "a.txt", "1\n");
        git(&repository, &["checkout", "-q", "-b", "feat/y"]);
        commit_file(&repository, "b.txt", "x\n");

        let reading = read(&repository, Some("main"));

        assert_eq!(reading.branch.as_deref(), Some("feat/y"));
        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 1,
                removed: 0
            })
        );
    }

    #[test]
    fn a_base_branch_that_does_not_exist_gives_no_changes() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);

        let reading = read(&clone, Some("release/9"));

        assert_eq!(reading.branch.as_deref(), Some("main"));
        assert_eq!(reading.changes, None);
    }

    #[test]
    fn without_a_named_base_the_remote_head_branch_is_the_base() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        git(&clone, &["checkout", "-q", "-b", "feat/x"]);
        commit_file(&clone, "b.txt", "x\n");

        let reading = read(&clone, None);

        assert_eq!(reading.base_branch.as_deref(), Some("main"));
        assert_eq!(
            reading.changes,
            Some(ChangedLines {
                added: 1,
                removed: 0
            })
        );
    }

    #[test]
    fn a_read_past_its_limit_gives_nothing_instead_of_waiting_for_git() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);

        let started = Instant::now();
        let reading = read_within(&clone, Some("main"), Duration::ZERO);

        assert_eq!(reading.branch, None);
        assert_eq!(reading.changes, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_binary_file_counts_as_no_lines() {
        assert_eq!(
            sum_numstat("-\t-\timage.png\n3\t1\ta.txt\n"),
            ChangedLines {
                added: 3,
                removed: 1
            }
        );
    }

    #[test]
    fn a_missing_directory_reads_as_nothing() {
        let scratch = Scratch::new();

        let reading = read(&scratch.0.join("gone"), Some("main"));

        assert_eq!(
            reading,
            GitReading {
                base_branch: Some("main".into()),
                ..GitReading::default()
            }
        );
    }

    #[test]
    fn reading_leaves_the_repository_unchanged() {
        let scratch = Scratch::new();
        let clone = origin_and_clone(&scratch);
        fs::write(clone.join("a.txt"), "changed\n").unwrap();
        let before = git(&clone, &["status", "--porcelain=v2"]);

        read(&clone, Some("main"));

        assert_eq!(git(&clone, &["status", "--porcelain=v2"]), before);
    }

    #[test]
    fn launch_headers_follow_the_workspace_order_with_the_start_branch() {
        let git_repository = |name: &str, remote: &str, registry_id: Option<&str>| TaskRepository {
            name: name.into(),
            branch: Some("release/2.0".into()),
            source: TaskRepositorySource::Git(Box::new(GitIdentity {
                repository: registry_id.map(str::to_owned),
                remote_url: remote.into(),
                commit: String::new(),
                git_ref: None,
                ref_origin: None,
                credential: None,
            })),
        };
        let workspace = TaskWorkspace {
            repositories: vec![
                git_repository("code", "https://git.example/g/orders-api.git", None),
                git_repository(
                    "docs",
                    "https://git.example/g/other.git",
                    Some("orders-docs"),
                ),
                TaskRepository::new(
                    "notes",
                    TaskRepositorySource::Local {
                        path: "/work/notes".into(),
                    },
                ),
            ],
        };

        let headers = launch_headers(&workspace);

        let names: Vec<_> = headers.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["code", "docs", "notes"]);
        assert_eq!(headers[0].repo, "orders-api");
        assert_eq!(headers[0].branch.as_deref(), Some("release/2.0"));
        assert_eq!(headers[0].base_branch.as_deref(), Some("release/2.0"));
        assert_eq!(headers[1].repo, "orders-docs");
        assert_eq!(headers[2].repo, "/work/notes");
        assert_eq!(headers[2].branch, None);
        assert_eq!(headers[2].base_branch, None);
    }

    #[test]
    fn a_repository_without_a_start_branch_has_only_its_name() {
        let workspace = TaskWorkspace::single(TaskRepository::new(
            "code",
            TaskRepositorySource::Git(Box::new(GitIdentity {
                repository: None,
                remote_url: "https://git.example/g/orders-api".into(),
                commit: String::new(),
                git_ref: None,
                ref_origin: None,
                credential: None,
            })),
        ));

        let headers = launch_headers(&workspace);

        assert_eq!(headers[0].repo, "orders-api");
        assert_eq!(headers[0].branch, None);
        assert_eq!(headers[0].base_branch, None);
    }
}
