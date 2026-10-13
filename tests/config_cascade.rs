//! Which configuration file wins (U3).
//!
//! # Nothing here touches the real machine
//!
//! Every test builds its cascade with [`ConfigCascade::with_dirs`], naming a
//! user directory and a system directory inside its own temporary root. The
//! developer's `~/.config` and the machine's `/etc` are therefore unreachable
//! from these tests, in both directions: they are never read and never
//! written. No test mutates the environment of the test process, so nothing
//! here can race a test running beside it.

use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::config::cascade::{CandidateKind, CascadeError, ConfigCascade};

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this
/// crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-cascade-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The directory standing in for `~/.config/narsor`.
    fn user_dir(&self) -> PathBuf {
        self.path.join("user")
    }

    /// The directory standing in for `/etc/narsor`.
    fn system_dir(&self) -> PathBuf {
        self.path.join("system")
    }

    /// Writes a file, creating its parents.
    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("create parents");
        fs::write(&path, contents).expect("write the file");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A cascade over the temporary root, with both directories enabled.
fn cascade(root: &TempRoot, named: Option<PathBuf>) -> ConfigCascade {
    ConfigCascade::with_dirs(named, Some(root.user_dir()), Some(root.system_dir()))
}

#[test]
fn a_file_in_the_user_directory_wins_and_carries_its_full_path() {
    let root = TempRoot::new();
    let path = root.write("user/config.yaml", "log_level: info\n");

    let resolution = cascade(&root, None).resolve().expect("resolve");

    let winner = resolution.winner.expect("a winner");
    assert_eq!(winner.path, path);
    assert_eq!(winner.kind, CandidateKind::User);
    assert!(resolution.shadowed.is_empty());
}

#[test]
fn the_user_file_wins_over_the_system_one_and_the_loser_is_named() {
    // AE5: the second file is not merged in, and it is not forgotten either.
    let root = TempRoot::new();
    let user = root.write("user/config.yaml", "log_level: info\n");
    let system = root.write("system/config.yaml", "log_level: debug\n");

    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.path(), Some(user.as_path()));
    let shadowed: Vec<&Path> = resolution
        .shadowed
        .iter()
        .map(|candidate| candidate.path.as_path())
        .collect();
    assert_eq!(
        shadowed,
        vec![system.as_path()],
        "the system file lost, and a listing has to be able to say so"
    );
}

#[test]
fn the_named_file_wins_over_both_directories() {
    let root = TempRoot::new();
    let named = root.write("elsewhere/mine.yaml", "log_level: warn\n");
    root.write("user/config.yaml", "log_level: info\n");
    root.write("system/config.yaml", "log_level: debug\n");

    let resolution = cascade(&root, Some(named.clone()))
        .resolve()
        .expect("resolve");

    assert_eq!(resolution.path(), Some(named.as_path()));
    assert_eq!(
        resolution.winner.expect("a winner").kind,
        CandidateKind::Flag
    );
    assert_eq!(resolution.shadowed.len(), 2, "both directories lost");
}

#[test]
fn no_file_anywhere_is_a_silent_normal_state() {
    // AE2 / R4: no home, no XDG, no /etc/narsor — and no complaint.
    let root = TempRoot::new();

    let resolution = ConfigCascade::with_dirs(None, None, None)
        .resolve()
        .expect("resolving without any directory must succeed");

    assert_eq!(resolution.winner, None);
    assert!(resolution.shadowed.is_empty());
    drop(root);
}

#[test]
fn a_directory_that_does_not_exist_is_simply_skipped() {
    let root = TempRoot::new();
    let system = root.write("system/config.yaml", "log_level: info\n");

    // The user directory is named but was never created.
    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.path(), Some(system.as_path()));
}

#[test]
fn the_yml_spelling_is_found_too() {
    let root = TempRoot::new();
    let path = root.write("user/config.yml", "log_level: info\n");

    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.path(), Some(path.as_path()));
}

#[test]
fn the_yaml_spelling_wins_over_the_yml_one_in_the_same_directory() {
    let root = TempRoot::new();
    let yaml = root.write("user/config.yaml", "log_level: info\n");
    let yml = root.write("user/config.yml", "log_level: debug\n");

    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.path(), Some(yaml.as_path()));
    assert!(resolution
        .shadowed
        .iter()
        .any(|candidate| candidate.path == yml));
}

#[test]
fn an_empty_file_is_a_perfectly_good_winner() {
    // D-H: an empty file means "every value built in", which is a statement,
    // not an absence.
    let root = TempRoot::new();
    let path = root.write("user/config.yaml", "");

    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.path(), Some(path.as_path()));
}

#[test]
fn a_directory_mounted_over_the_file_path_stops_the_start() {
    // The shape a ConfigMap arrives in when the volume is mounted one level
    // too high.
    let root = TempRoot::new();
    fs::create_dir_all(root.system_dir().join("config.yaml")).expect("create the directory");

    let error = cascade(&root, None)
        .resolve()
        .expect_err("a directory under the file path is not a configuration");

    assert!(matches!(error, CascadeError::NotAFile { .. }));
    let message = error.to_string();
    assert!(
        message.contains("config.yaml") && message.contains("directory"),
        "the message must name the path and say what is wrong: {message}"
    );
}

#[test]
fn a_broken_symbolic_link_stops_the_start() {
    let root = TempRoot::new();
    fs::create_dir_all(root.user_dir()).expect("create the user directory");
    let link = root.user_dir().join("config.yaml");
    std::os::unix::fs::symlink(root.path().join("gone.yaml"), &link).expect("create the link");

    let error = cascade(&root, None)
        .resolve()
        .expect_err("a dangling link is not an absent file");

    assert!(matches!(error, CascadeError::BrokenLink { .. }));
    assert!(error.to_string().contains("config.yaml"));
}

#[test]
fn a_file_that_cannot_be_read_says_so_rather_than_claiming_it_is_missing() {
    use std::os::unix::fs::PermissionsExt;

    let root = TempRoot::new();
    let path = root.write("user/config.yaml", "log_level: info\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("drop the permissions");

    let outcome = cascade(&root, None).resolve();

    // A test run as root can read anything; the assertion only holds where the
    // permission actually bites.
    if fs::File::open(&path).is_ok() {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).ok();
        return;
    }

    let error = outcome.expect_err("an unreadable file is not an absent one");
    assert!(matches!(error, CascadeError::Unreadable { .. }));
    assert!(error.to_string().contains("config.yaml"));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).ok();
}

#[test]
fn a_named_file_that_is_not_there_is_a_usage_error_not_a_fall_through() {
    let root = TempRoot::new();
    let user = root.write("user/config.yaml", "log_level: info\n");
    let missing = root.path().join("nope.yaml");

    let error = cascade(&root, Some(missing.clone()))
        .resolve()
        .expect_err("the operator named a file that is not there");

    assert!(matches!(error, CascadeError::MissingNamedFile { .. }));
    assert!(error.is_usage(), "this one is a usage error, exit code 2");
    assert!(error.to_string().contains("nope.yaml"));
    assert!(
        user.exists(),
        "the user file is still there — it was simply never reached"
    );
}

#[test]
fn a_broken_candidate_below_the_winner_does_not_stop_the_start() {
    let root = TempRoot::new();
    let user = root.write("user/config.yaml", "log_level: info\n");
    fs::create_dir_all(root.system_dir().join("config.yaml")).expect("create the directory");

    let resolution = cascade(&root, None)
        .resolve()
        .expect("the winner was found before the broken candidate was reached");

    assert_eq!(resolution.path(), Some(user.as_path()));
}

#[test]
fn resolving_creates_nothing_at_all() {
    // R9: no writable home, no writable filesystem — and nothing to write.
    let root = TempRoot::new();
    let user = root.user_dir();
    let system = root.system_dir();

    let resolution = cascade(&root, None).resolve().expect("resolve");

    assert_eq!(resolution.winner, None);
    assert!(!user.exists(), "the user directory must not be created");
    assert!(!system.exists(), "the system directory must not be created");
}

#[test]
fn the_winning_path_is_absolute_even_when_the_directory_was_relative() {
    // R8: "which file did this come from" is only half answered by
    // "config.yaml". A relative XDG_CONFIG_HOME is against the spec but is
    // accepted by the code around this, so it has to be handled here.
    let root = TempRoot::new();
    root.write("user/config.yaml", "log_level: info\n");

    let relative = pathdiff_from_cwd(&root.user_dir());
    let resolution = ConfigCascade::with_dirs(None, Some(relative), None)
        .resolve()
        .expect("resolve");

    let path = resolution.path().expect("a winner");
    assert!(
        path.is_absolute(),
        "the reported path must be absolute: {}",
        path.display()
    );
    assert!(path.ends_with("user/config.yaml"));
}

/// The path of `target` written relative to the current directory.
fn pathdiff_from_cwd(target: &Path) -> PathBuf {
    let cwd = std::env::current_dir().expect("a working directory");
    let mut relative = PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    for component in target.components().skip(1) {
        relative.push(component);
    }
    relative
}

#[test]
fn a_config_map_symlink_chain_is_reported_by_the_path_that_was_mounted() {
    // A ConfigMap mounts config.yaml -> ..data/config.yaml -> ..2026_.../config.yaml,
    // and replaces the snapshot directory on every update. Reporting the
    // snapshot would name a path that is real for a few seconds and is not the
    // one the operator mounted (R8).
    let root = TempRoot::new();
    let system = root.system_dir();
    let snapshot = system.join("..2026_09_24_10_00_00.1234");
    fs::create_dir_all(&snapshot).expect("create the snapshot directory");
    fs::write(snapshot.join("config.yaml"), "log_level: info\n").expect("write the file");
    std::os::unix::fs::symlink(&snapshot, system.join("..data")).expect("link ..data");
    std::os::unix::fs::symlink(
        Path::new("..data").join("config.yaml"),
        system.join("config.yaml"),
    )
    .expect("link config.yaml");

    let resolution = ConfigCascade::with_dirs(None, None, Some(system.clone()))
        .resolve()
        .expect("resolve");

    let path = resolution.path().expect("a winner");
    assert_eq!(
        path,
        system.join("config.yaml"),
        "the mounted path, not the snapshot it currently points at"
    );
    assert!(
        !path.to_string_lossy().contains("..2026_"),
        "the snapshot directory is replaced on the next update: {}",
        path.display()
    );
}
