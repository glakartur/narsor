//! The checkpoint: the difference between a pinned base and the work tree.
//!
//! # Why a git bundle and not a diff
//!
//! A checkpoint has to carry binary files, mode changes, deletions and renames
//! — everything a work tree can hold. That is precisely the set git already
//! serialises, so the checkpoint is a `git bundle` of one commit, thin against
//! the base (`--not <base>`), and what travels is the delta and nothing else.
//!
//! # Why the index is temporary
//!
//! Producing the commit stages the whole work tree, which would destroy the
//! staging area the person is working in. `GIT_INDEX_FILE` points the operation
//! at an index outside the work tree, so the user's index is never touched.
//!
//! # Why the temporary index is seeded from the base
//!
//! An *empty* temporary index makes `.gitignore` authoritative over
//! tracked-ness: a file that is tracked in the base and later added to
//! `.gitignore` is simply not staged, vanishes from the checkpoint, and is
//! **deleted** when the checkpoint is restored. Seeding with `read-tree <base>`
//! keeps tracked files tracked, and leaves the ignore rules governing only what
//! is genuinely new — which is what they are for.
//!
//! # The same mechanism for a directory that is not a repository
//!
//! [`Base::None`] points `GIT_DIR` at a scratch repository beside the execution
//! and `GIT_WORK_TREE` at the directory, so a workflow with no repository at
//! all (`DOMAIN_MODEL.md` §2.8, D39) checkpoints through this very code with no
//! second path — and without a `.git` appearing in its workspace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// The ref a checkpoint commit is published under, inside the bundle.
pub const CHECKPOINT_REF: &str = "refs/narsor/checkpoint";

/// What the delta is measured against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// A git work tree whose object store already holds this commit.
    Commit(String),
    /// A directory that is not a git work tree; the whole content is the delta.
    None,
}

/// Everything [`capture`] needs.
#[derive(Debug)]
pub struct CaptureRequest<'a> {
    /// The directory whose contents are captured.
    pub work_tree: &'a Path,
    /// The base the delta is measured from.
    pub base: Base,
    /// Git's scratch area: the temporary index, and the repository for
    /// [`Base::None`]. Created if missing.
    pub scratch: &'a Path,
    /// Where the bundle is written. Replaced atomically.
    pub bundle: &'a Path,
    /// Paths force-added despite `.gitignore` (R24's only exception).
    pub preserve: &'a [String],
}

/// What one capture produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The checkpoint commit.
    pub commit: String,
    /// Size of the bundle on disk.
    pub bytes: u64,
}

/// Everything that can stop a checkpoint from being produced or applied.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// A `git` invocation failed.
    #[error("git {command} failed: {stderr}")]
    Git {
        /// The subcommand, for the message.
        command: String,
        /// What git said.
        stderr: String,
    },

    /// The base commit is not in the repository the checkpoint is applied to.
    ///
    /// The usual cause is a force-push or a `gc` on the remote between the
    /// checkpoint being taken and being restored.
    #[error("the base commit {commit} is not present; the remote may have been force-pushed")]
    MissingBase {
        /// The commit that could not be found.
        commit: String,
    },

    /// A filesystem operation failed.
    #[error("checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Runs one `git` invocation with the checkpoint's environment.
fn run(
    work_tree: &Path,
    git_dir: Option<&Path>,
    index: Option<&Path>,
    args: &[&str],
) -> Result<String, CheckpointError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(work_tree)
        .args(["-c", "user.name=narsor"])
        .args(["-c", "user.email=narsor@localhost"])
        .args(["-c", "commit.gpgsign=false"])
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");

    if let Some(dir) = git_dir {
        command.env("GIT_DIR", dir).env("GIT_WORK_TREE", work_tree);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }

    let output = command.output()?;
    if !output.status.success() {
        return Err(CheckpointError::Git {
            command: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Sets up `GIT_DIR` for `base`, creating and initialising the scratch
/// repository beside `work_tree` the first time [`Base::None`] is seen.
///
/// Shared by [`capture`] and [`materialise`], which otherwise repeat this
/// bootstrap identically.
fn git_dir_for(
    base: &Base,
    work_tree: &Path,
    scratch: &Path,
) -> Result<Option<PathBuf>, CheckpointError> {
    match base {
        Base::Commit(_) => Ok(None),
        Base::None => {
            let dir = scratch.join("repo.git");
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
                run(work_tree, Some(&dir), None, &["init", "--quiet"])?;
            }
            Ok(Some(dir))
        }
    }
}

/// Writes the checkpoint of `work_tree` to `bundle`, replacing any previous one.
///
/// The bundle is written to a unique temporary name in the same directory and
/// renamed into place, so an interrupted or failing capture leaves the previous
/// checkpoint byte-identical (R25).
pub fn capture(request: CaptureRequest<'_>) -> Result<Capture, CheckpointError> {
    fs::create_dir_all(request.scratch)?;
    let index = request.scratch.join("index");
    let _ = fs::remove_file(&index);

    let git_dir = git_dir_for(&request.base, request.work_tree, request.scratch)?;
    let git_dir = git_dir.as_deref();

    if let Base::Commit(commit) = &request.base {
        run(
            request.work_tree,
            git_dir,
            Some(&index),
            &["read-tree", commit],
        )
        .map_err(|_| CheckpointError::MissingBase {
            commit: commit.clone(),
        })?;
    }

    run(
        request.work_tree,
        git_dir,
        Some(&index),
        &["add", "--all", "--", "."],
    )?;
    for path in request.preserve {
        run(
            request.work_tree,
            git_dir,
            Some(&index),
            &["add", "--force", "--", path.as_str()],
        )?;
    }

    let tree = run(request.work_tree, git_dir, Some(&index), &["write-tree"])?;

    let mut commit_args = vec!["commit-tree".to_string(), tree];
    if let Base::Commit(base) = &request.base {
        commit_args.push("-p".to_string());
        commit_args.push(base.clone());
    }
    commit_args.push("-m".to_string());
    commit_args.push("narsor checkpoint".to_string());
    let commit_argv: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    let commit = run(request.work_tree, git_dir, Some(&index), &commit_argv)?;

    run(
        request.work_tree,
        git_dir,
        Some(&index),
        &["update-ref", CHECKPOINT_REF, &commit],
    )?;

    let parent = request
        .bundle
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)?;
    let staging = parent.join(format!("checkpoint-{}.bundle", uuid::Uuid::new_v4()));

    let mut bundle_args = vec![
        "bundle".to_string(),
        "create".to_string(),
        "--quiet".to_string(),
        staging.display().to_string(),
        CHECKPOINT_REF.to_string(),
    ];
    if let Base::Commit(base) = &request.base {
        bundle_args.push("--not".to_string());
        bundle_args.push(base.clone());
    }
    let bundle_argv: Vec<&str> = bundle_args.iter().map(String::as_str).collect();

    if let Err(error) = run(request.work_tree, git_dir, Some(&index), &bundle_argv) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }

    let bytes = fs::metadata(&staging)?.len();
    fs::rename(&staging, request.bundle)?;

    Ok(Capture { commit, bytes })
}

/// Everything [`materialise`] needs.
#[derive(Debug)]
pub struct MaterialiseRequest<'a> {
    /// The directory the checkpoint is restored into. For [`Base::Commit`] it
    /// must already be a checkout holding that commit.
    pub work_tree: &'a Path,
    /// The base the delta was measured from.
    pub base: Base,
    /// Git's scratch area, as in [`CaptureRequest`].
    pub scratch: &'a Path,
    /// The bundle to restore.
    pub bundle: &'a Path,
}

/// Restores a checkpoint into `work_tree`.
///
/// The work tree ends up holding exactly what it held when the checkpoint was
/// taken, and `git status` reports the same dirty state: the checkpoint commit
/// is checked out hard, then the index is reset back to the base, which turns
/// its content back into unstaged modifications and untracked files.
pub fn materialise(request: MaterialiseRequest<'_>) -> Result<(), CheckpointError> {
    fs::create_dir_all(request.scratch)?;

    let git_dir = git_dir_for(&request.base, request.work_tree, request.scratch)?;
    let git_dir = git_dir.as_deref();

    if let Base::Commit(commit) = &request.base {
        run(
            request.work_tree,
            git_dir,
            None,
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        )
        .map_err(|_| CheckpointError::MissingBase {
            commit: commit.clone(),
        })?;
    }

    let refspec = format!("{CHECKPOINT_REF}:{CHECKPOINT_REF}");
    run(
        request.work_tree,
        git_dir,
        None,
        &[
            "fetch",
            "--quiet",
            &request.bundle.display().to_string(),
            &refspec,
        ],
    )?;

    run(
        request.work_tree,
        git_dir,
        None,
        &["reset", "--quiet", "--hard", CHECKPOINT_REF],
    )?;

    if let Base::Commit(base) = &request.base {
        run(
            request.work_tree,
            git_dir,
            None,
            &["reset", "--quiet", "--mixed", base],
        )?;
    }

    Ok(())
}
