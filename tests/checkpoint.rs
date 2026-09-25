//! The checkpoint: a git bundle of the delta between a pinned base and the
//! work tree as it stands.
//!
//! Everything runs against a real filesystem and a real `git`, in a temporary
//! directory — never inside the repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use narsor::workspace::{capture, Base, CaptureRequest};

/// A temporary directory removed when it goes away.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-checkpoint-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Checkpoint Test"])
        .args(["-c", "user.email=checkpoint@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository with one commit, and the sha of that commit.
fn repo_with_base(dir: &Path) -> String {
    fs::create_dir_all(dir).expect("create repo");
    git(dir, &["-c", "init.defaultBranch=main", "init"]);
    fs::write(dir.join("tracked.txt"), "base\n").expect("write tracked");
    fs::write(dir.join(".gitignore"), "ignored.log\n").expect("write gitignore");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--message", "base"]);
    git(dir, &["rev-parse", "HEAD"])
}

#[test]
fn capture_leaves_the_working_index_untouched() {
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify");
    fs::write(work.join("untracked.txt"), "new\n").expect("add untracked");

    let before = git(&work, &["status", "--porcelain"]);

    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    assert_eq!(
        git(&work, &["status", "--porcelain"]),
        before,
        "capture must not disturb the user's index or work tree"
    );
}

#[test]
fn capture_excludes_ignored_files() {
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("ignored.log"), "noise\n").expect("write ignored");

    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        !listing.contains("ignored.log"),
        "an ignored file must not travel in the checkpoint, got: {listing}"
    );
}

#[test]
fn capture_keeps_a_tracked_file_that_became_ignored() {
    // Review Focus 1. `.gitignore` governs files that are new, not files that
    // are already tracked. Seeding the temporary index from the base is what
    // makes this true; an empty index silently drops the file, and restoring
    // then deletes it.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("kept.txt"), "tracked\n").expect("write kept");
    git(&work, &["add", "--", "kept.txt"]);
    git(&work, &["commit", "--message", "track kept.txt"]);
    fs::write(work.join(".gitignore"), "ignored.log\nkept.txt\n").expect("ignore kept.txt");
    fs::write(work.join("kept.txt"), "tracked and edited\n").expect("edit kept");

    let head = git(&work, &["rev-parse", "HEAD"]);
    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(head),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &[],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        listing.contains("kept.txt"),
        "a tracked file added to .gitignore must stay in the checkpoint, got: {listing}"
    );
}

#[test]
fn a_failed_capture_leaves_the_previous_bundle_intact() {
    // Review Focus 2.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);
    let bundle = temp.join("cp.bundle");

    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("first capture");

    let good = fs::read(&bundle).expect("read first bundle");

    // A base that does not exist makes git fail part-way.
    let failed = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit("0".repeat(40)),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    });

    assert!(failed.is_err(), "capture against a missing base must fail");
    assert_eq!(
        fs::read(&bundle).expect("read bundle after the failure"),
        good,
        "a failed capture must leave the previous checkpoint byte-identical"
    );
}
