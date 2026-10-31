//! The checkpoint: a git bundle of the delta between a pinned base and the
//! work tree as it stands.
//!
//! Everything runs against a real filesystem and a real `git`, in a temporary
//! directory — never inside the repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use narsor_engine::workspace::{capture, materialise, Base, CaptureRequest, MaterialiseRequest};

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
fn preserve_paths_carries_an_ignored_file() {
    // R24's only exception: a generated file that the repository ignores but
    // the workflow needs — the classic case being a `.env` written by a step.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("ignored.log"), "needed\n").expect("write ignored");

    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &["ignored.log".to_string()],
    })
    .expect("capture");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        listing.contains("ignored.log"),
        "a preserved path must travel despite .gitignore, got: {listing}"
    );
}

#[test]
fn preserve_paths_skips_an_entry_that_does_not_exist() {
    // A preserve entry naming a path that has not been written yet (e.g. a
    // `.env` an earlier step has not created) must not abort the whole
    // capture: `git add --force` on a pathspec matching nothing exits
    // non-zero, and that must be swallowed for exactly that entry while every
    // other preserved path still travels.
    let temp = TempRoot::new();
    let work = temp.join("work");
    let base = repo_with_base(&work);

    fs::write(work.join("ignored.log"), "needed\n").expect("write ignored");

    let capture_result = capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base),
        scratch: &temp.join("scratch"),
        bundle: &temp.join("cp.bundle"),
        preserve: &["ignored.log".to_string(), "missing.env".to_string()],
    })
    .expect("capture must succeed even though one preserve path is missing");

    let listing = git(
        &work,
        &["ls-tree", "-r", "--name-only", &capture_result.commit],
    );
    assert!(
        listing.contains("ignored.log"),
        "the existing preserved path must still travel, got: {listing}"
    );
    assert!(
        !listing.contains("missing.env"),
        "the missing preserved path cannot appear, got: {listing}"
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
    let _base = repo_with_base(&work);

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

/// Clones `origin` into `into` and checks `base` out, the way a takeover does.
fn clone_at(origin: &Path, into: &Path, base: &str) {
    let parent = into.parent().expect("clone target has a parent");
    fs::create_dir_all(parent).expect("create clone parent");
    let output = Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg(origin)
        .arg(into)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git clone");
    assert!(output.status.success(), "git clone failed");
    git(into, &["checkout", "--quiet", base]);
}

#[test]
fn a_dirty_tree_survives_a_round_trip() {
    // Success criterion 3: the restored tree is identical to the captured one,
    // and `git status` reports the same dirty state, not a pile of staged work.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);

    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify tracked");
    fs::write(work.join("untracked.txt"), "brand new\n").expect("add untracked");
    fs::write(work.join("binary.dat"), [0u8, 1, 2, 255]).expect("add binary");
    fs::remove_file(work.join(".gitignore")).expect("delete a tracked file");
    let expected_status = git(&work, &["status", "--porcelain"]);

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    let fresh = temp.join("fresh");
    clone_at(&origin, &fresh, &base);
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::Commit(base),
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise");

    assert_eq!(
        git(&fresh, &["status", "--porcelain"]),
        expected_status,
        "the restored tree must show the same dirty state"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("tracked.txt")).expect("read tracked"),
        "base\nmodified\n"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("untracked.txt")).expect("read untracked"),
        "brand new\n"
    );
    assert_eq!(
        fs::read(fresh.join("binary.dat")).expect("read binary"),
        vec![0u8, 1, 2, 255]
    );
    assert!(
        !fresh.join(".gitignore").exists(),
        "the deletion must travel"
    );
}

#[test]
fn materialising_without_the_base_is_a_named_error() {
    // Review Focus 3: a force-pushed or garbage-collected base must produce a
    // sentence an operator can act on, not a wall of git stderr.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);
    fs::write(work.join("tracked.txt"), "base\nmodified\n").expect("modify");

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    // A fresh repository that has never heard of the base.
    let stranger = temp.join("stranger");
    fs::create_dir_all(&stranger).expect("create stranger");
    git(&stranger, &["-c", "init.defaultBranch=main", "init"]);

    let error = materialise(MaterialiseRequest {
        work_tree: &stranger,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("stranger-scratch"),
        bundle: &bundle,
    })
    .expect_err("materialising without the base must fail");

    let message = error.to_string();
    assert!(
        message.contains(&base) && message.contains("force-pushed"),
        "expected a named MissingBase error, got: {message}"
    );
}

#[test]
fn a_nested_repository_restores_as_an_empty_directory() {
    // Review Focus 5. `git add -A` records a nested repository as a gitlink, so
    // the restore leaves the directory empty. Pinned here so the behaviour is
    // known rather than discovered in production.
    let temp = TempRoot::new();
    let origin = temp.join("origin");
    let base = repo_with_base(&origin);

    let work = temp.join("work");
    clone_at(&origin, &work, &base);

    let nested = work.join("vendor");
    fs::create_dir_all(&nested).expect("create nested");
    git(&nested, &["-c", "init.defaultBranch=main", "init"]);
    fs::write(nested.join("inner.txt"), "inner\n").expect("write inner");
    git(&nested, &["add", "-A"]);
    git(&nested, &["commit", "--message", "inner"]);

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::Commit(base.clone()),
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture");

    let fresh = temp.join("fresh");
    clone_at(&origin, &fresh, &base);
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::Commit(base),
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise");

    assert!(
        !fresh.join("vendor").join("inner.txt").exists(),
        "a nested repository's contents do not travel in the checkpoint"
    );

    let vendor = fresh.join("vendor");
    assert!(
        vendor.exists() && vendor.is_dir(),
        "the gitlink still recreates the directory on restore"
    );
    assert!(
        fs::read_dir(&vendor).expect("read vendor").next().is_none(),
        "the recreated directory is empty"
    );
}

#[test]
fn a_plain_directory_round_trips_without_becoming_a_repository() {
    // AE8 / Review Focus 4: a workflow that declares no workspace gets an empty
    // directory (docs/dev/domain-model.md §2.8). It must checkpoint through the very same
    // code — and must not grow a `.git` of its own.
    let temp = TempRoot::new();
    let work = temp.join("plain");
    fs::create_dir_all(work.join("sub")).expect("create plain workspace");
    fs::write(work.join("a.txt"), "hello\n").expect("write a");
    fs::write(work.join("sub").join("b.dat"), [0u8, 9, 9]).expect("write b");

    let bundle = temp.join("cp.bundle");
    capture(CaptureRequest {
        work_tree: &work,
        base: Base::None,
        scratch: &temp.join("scratch"),
        bundle: &bundle,
        preserve: &[],
    })
    .expect("capture a plain directory");

    assert!(
        !work.join(".git").exists(),
        "checkpointing must not turn the workspace into a repository"
    );

    let fresh = temp.join("fresh-plain");
    fs::create_dir_all(&fresh).expect("create fresh");
    materialise(MaterialiseRequest {
        work_tree: &fresh,
        base: Base::None,
        scratch: &temp.join("fresh-scratch"),
        bundle: &bundle,
    })
    .expect("materialise a plain directory");

    assert_eq!(
        fs::read_to_string(fresh.join("a.txt")).expect("read a"),
        "hello\n"
    );
    assert_eq!(
        fs::read(fresh.join("sub").join("b.dat")).expect("read b"),
        vec![0u8, 9, 9]
    );
    assert!(!fresh.join(".git").exists(), "no .git in the restored tree");
}
