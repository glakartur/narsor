//! The Claude Code profile a task's harness starts with (R11-R14, KTD6).
//!
//! A profile has a *source* directory — in Kubernetes the read-only mount of
//! the dedicated profile Secret — and a *work* directory, the writable
//! `CLAUDE_CONFIG_DIR` the harness runs on. When there is no source, or the
//! source is the work directory itself (a local installation), the harness
//! runs on its directory as it is and nothing is copied.
//!
//! Otherwise the runner brings the work directory up to date before every
//! start with a three-way merge per file:
//!
//! * the **base** — the copy of the source taken at the previous start,
//!   kept in the runner's bookkeeping;
//! * the **local** version — what the harness left in the work directory;
//! * the current **source**.
//!
//! A file the harness did not change takes the source's version (a new OAuth
//! session reaches the harness); a file the source did not change keeps the
//! harness's. A file changed on both sides is settled by its type: the
//! credentials file is taken from the source whole, because tokens of two
//! sessions must not mix; other JSON files are merged key by key, lists
//! (permission rules) are united and a value changed on both sides is the
//! source's; any other file is the source's. Files the source never had —
//! sessions, history, todos — are left alone. Nothing is ever written back
//! to the source (R14).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{HarnessHome, HomeMode, RunnerError};

/// Directory name Claude Code keeps its configuration in under `$HOME`.
const STANDARD_DIR: &str = ".claude";

/// The file holding Claude Code's login.
const CREDENTIALS_FILE: &str = ".credentials.json";

/// Prefix of the entries a Secret volume keeps for itself (`..data`,
/// `..2026_10_18_…`): its machinery, not part of the profile.
const VOLUME_INTERNAL_PREFIX: &str = "..";

/// Prepares the Claude Code work directory of this start.
///
/// Returns the work directory the harness must be pointed at, `None` when
/// there is no profile source and the harness keeps whatever directory the
/// launch configured. `private_home` is the task's private HOME, used for the
/// work directory when none is configured; `base` is the bookkeeping
/// directory holding the copy of the source taken at the previous start.
pub fn prepare_claude_profile(
    home: &HarnessHome,
    private_home: &Path,
    base: &Path,
) -> Result<Option<PathBuf>, RunnerError> {
    let Some(source) = &home.claude_profile_source else {
        return Ok(None);
    };
    let work = match (&home.claude_config_dir, home.mode) {
        (Some(dir), _) => dir.clone(),
        (None, HomeMode::Private) => private_home.join(STANDARD_DIR),
        (None, HomeMode::Host) => {
            return Err(RunnerError::Profile(
                "a Claude profile source needs a work directory: configure the Claude \
                 configuration directory or a private HOME"
                    .into(),
            ))
        }
    };
    if !source.is_dir() {
        return Err(RunnerError::Profile(format!(
            "the Claude profile source '{}' is not a directory",
            source.display()
        )));
    }
    if !same_directory(source, &work) {
        merge_profile(source, &work, base)?;
    }
    Ok(Some(work))
}

fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Brings `work` up to date with `source`, then records `source` as the
/// new `base`. Safe to repeat after a crash: a file already brought up to
/// date compares equal to the source and is left alone.
pub fn merge_profile(source: &Path, work: &Path, base: &Path) -> Result<(), RunnerError> {
    let source_files = read_tree(source)?;
    let base_files = if base.is_dir() {
        read_tree(base)?
    } else {
        BTreeMap::new()
    };
    create_private_dir(work)?;

    let paths: BTreeSet<&PathBuf> = source_files.keys().chain(base_files.keys()).collect();
    for relative in paths {
        let target = work.join(relative);
        let local = read_local(&target)?;
        let base = base_files.get(relative).map(|file| file.bytes.as_slice());
        let incoming = source_files.get(relative);
        let theirs = incoming.map(|file| file.bytes.as_slice());
        let ours = local.as_deref();
        if theirs == base || ours == theirs {
            continue;
        }
        let merged = if ours == base {
            theirs.map(<[u8]>::to_vec)
        } else {
            settle_conflict(relative, base, ours, theirs)
        };
        match merged {
            Some(bytes) => write_private(
                work,
                &target,
                &bytes,
                incoming.is_some_and(|file| file.executable),
            )?,
            None => remove_file(&target)?,
        }
    }

    replace_base(base, &source_files)
}

/// The version of a file changed both by the harness and in the source.
fn settle_conflict(
    relative: &Path,
    base: Option<&[u8]>,
    local: Option<&[u8]>,
    source: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let is_credentials = relative
        .file_name()
        .is_some_and(|name| name == CREDENTIALS_FILE);
    let is_json = relative
        .extension()
        .is_some_and(|extension| extension == "json");
    if is_credentials || !is_json {
        return source.map(<[u8]>::to_vec);
    }
    let parse = |bytes: Option<&[u8]>| bytes.and_then(|bytes| serde_json::from_slice(bytes).ok());
    let (Some(local), Some(source)) = (parse(local), parse(source)) else {
        return source.map(<[u8]>::to_vec);
    };
    let base: Option<Value> = parse(base);
    let merged = merge_json(base.as_ref(), Some(&local), Some(&source))?;
    serde_json::to_vec_pretty(&merged).ok()
}

/// Three-way merge of two JSON values against their common base: what one
/// side changed wins over the other side's unchanged value; objects merge
/// key by key; lists keep the local entries the source did not drop and add
/// the entries the source added; anything else changed on both sides is the
/// source's. `None` is an absent key.
pub fn merge_json(
    base: Option<&Value>,
    local: Option<&Value>,
    source: Option<&Value>,
) -> Option<Value> {
    if local == source || source == base {
        return local.cloned();
    }
    if local == base {
        return source.cloned();
    }
    match (local, source) {
        (Some(Value::Object(local)), Some(Value::Object(source))) => {
            let base = base.and_then(Value::as_object);
            let mut merged = Map::new();
            for key in local.keys().chain(source.keys()) {
                if merged.contains_key(key) {
                    continue;
                }
                let value = merge_json(
                    base.and_then(|base| base.get(key)),
                    local.get(key),
                    source.get(key),
                );
                if let Some(value) = value {
                    merged.insert(key.clone(), value);
                }
            }
            Some(Value::Object(merged))
        }
        (Some(Value::Array(local)), Some(Value::Array(source))) => {
            let empty = Vec::new();
            let base = base.and_then(Value::as_array).unwrap_or(&empty);
            let mut merged: Vec<Value> = local
                .iter()
                .filter(|entry| !base.contains(entry) || source.contains(entry))
                .cloned()
                .collect();
            for entry in source {
                if !base.contains(entry) && !merged.contains(entry) {
                    merged.push(entry.clone());
                }
            }
            Some(Value::Array(merged))
        }
        _ => source.cloned(),
    }
}

/// One file of a profile tree.
struct ProfileFile {
    bytes: Vec<u8>,
    executable: bool,
}

/// Every regular file under `root`, by its path relative to `root`,
/// following symbolic links (a Secret volume is made of them) and skipping
/// the volume's own entries.
fn read_tree(root: &Path) -> Result<BTreeMap<PathBuf, ProfileFile>, RunnerError> {
    fn walk(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<PathBuf, ProfileFile>,
    ) -> Result<(), RunnerError> {
        let entries = fs::read_dir(dir).map_err(|source| RunnerError::io(dir, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| RunnerError::io(dir, source))?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(VOLUME_INTERNAL_PREFIX)
            {
                continue;
            }
            let path = entry.path();
            let metadata = fs::metadata(&path).map_err(|source| RunnerError::io(&path, source))?;
            if metadata.is_dir() {
                walk(root, &path, files)?;
            } else if metadata.is_file() {
                let bytes = fs::read(&path).map_err(|source| RunnerError::io(&path, source))?;
                let relative = path
                    .strip_prefix(root)
                    .expect("a walked path lies under its root")
                    .to_path_buf();
                let executable = metadata.permissions().mode() & 0o111 != 0;
                files.insert(relative, ProfileFile { bytes, executable });
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    Ok(files)
}

fn read_local(path: &Path) -> Result<Option<Vec<u8>>, RunnerError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => fs::read(path)
            .map(Some)
            .map_err(|source| RunnerError::io(path, source)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RunnerError::io(path, source)),
    }
}

/// Writes a profile file readable by its owner only — it may hold a login —
/// through a sibling temporary file, so the harness never reads half of it.
fn write_private(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    executable: bool,
) -> Result<(), RunnerError> {
    let parent = path.parent().unwrap_or(root);
    create_private_dir(parent)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = parent.join(format!(".{name}.narsor-partial"));
    let mode = if executable { 0o700 } else { 0o600 };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|source| RunnerError::io(&temporary, source))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| RunnerError::io(&temporary, source))?;
    // `mode` applies only to a file the open created.
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))
        .map_err(|source| RunnerError::io(&temporary, source))?;
    fs::rename(&temporary, path).map_err(|source| RunnerError::io(path, source))
}

fn remove_file(path: &Path) -> Result<(), RunnerError> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(RunnerError::io(path, error))
        }
        _ => Ok(()),
    }
}

fn create_private_dir(path: &Path) -> Result<(), RunnerError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|source| RunnerError::io(path, source))
}

/// Records the source as the base of the next start: written aside, then
/// swapped in, so a crash leaves either the old base or the new one.
fn replace_base(base: &Path, files: &BTreeMap<PathBuf, ProfileFile>) -> Result<(), RunnerError> {
    let name = base.file_name().unwrap_or_default().to_string_lossy();
    let partial = base.with_file_name(format!("{name}.partial"));
    remove_tree(&partial)?;
    create_private_dir(&partial)?;
    for (relative, file) in files {
        write_private(
            &partial,
            &partial.join(relative),
            &file.bytes,
            file.executable,
        )?;
    }
    remove_tree(base)?;
    fs::rename(&partial, base).map_err(|source| RunnerError::io(base, source))
}

fn remove_tree(path: &Path) -> Result<(), RunnerError> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(RunnerError::io(path, error))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-profile-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("source")).unwrap();
            Self { root }
        }

        fn source(&self) -> PathBuf {
            self.root.join("source")
        }

        fn work(&self) -> PathBuf {
            self.root.join("home/.claude")
        }

        fn base(&self) -> PathBuf {
            self.root.join("narsor/claude-profile")
        }

        fn put(&self, dir: PathBuf, relative: &str, content: &str) {
            let path = dir.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.work().join(relative)).unwrap()
        }

        fn read_json(&self, relative: &str) -> Value {
            serde_json::from_str(&self.read(relative)).unwrap()
        }

        fn merge(&self) {
            merge_profile(&self.source(), &self.work(), &self.base()).unwrap();
        }

        fn home(
            &self,
            source: Option<PathBuf>,
            work: Option<PathBuf>,
            mode: HomeMode,
        ) -> HarnessHome {
            HarnessHome {
                mode,
                codex_home: None,
                claude_config_dir: work,
                claude_profile_source: source,
                agent_args: Vec::new(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn without_a_source_nothing_is_copied() {
        let fixture = Fixture::new();
        let home = fixture.home(None, Some(fixture.work()), HomeMode::Host);
        let work = prepare_claude_profile(&home, &fixture.root.join("home"), &fixture.base());
        assert!(work.unwrap().is_none());
        assert!(!fixture.work().exists());
        assert!(!fixture.base().exists());
    }

    #[test]
    fn a_source_equal_to_the_work_directory_is_not_copied() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), "settings.json", "{}");
        let home = fixture.home(
            Some(fixture.source()),
            Some(fixture.source()),
            HomeMode::Host,
        );
        let work =
            prepare_claude_profile(&home, &fixture.root.join("home"), &fixture.base()).unwrap();
        assert_eq!(work, Some(fixture.source()));
        assert!(!fixture.base().exists(), "no merge bookkeeping");
    }

    #[test]
    fn a_source_without_a_work_directory_on_the_hosts_home_is_refused() {
        let fixture = Fixture::new();
        let home = fixture.home(Some(fixture.source()), None, HomeMode::Host);
        let error =
            prepare_claude_profile(&home, &fixture.root.join("home"), &fixture.base()).unwrap_err();
        assert!(error.to_string().contains("work directory"), "{error}");
    }

    #[test]
    fn a_missing_source_is_refused() {
        let fixture = Fixture::new();
        let home = fixture.home(Some(fixture.root.join("absent")), None, HomeMode::Private);
        assert!(
            prepare_claude_profile(&home, &fixture.root.join("home"), &fixture.base()).is_err()
        );
    }

    #[test]
    fn a_private_home_holds_the_work_directory_by_default() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), "settings.json", "{}");
        let home = fixture.home(Some(fixture.source()), None, HomeMode::Private);
        let work =
            prepare_claude_profile(&home, &fixture.root.join("home"), &fixture.base()).unwrap();
        assert_eq!(work, Some(fixture.work()));
        assert_eq!(fixture.read("settings.json"), "{}");
    }

    #[test]
    fn a_fresh_task_copies_every_profile_file_but_the_volumes_own_entries() {
        let fixture = Fixture::new();
        let source = fixture.source();
        fixture.put(source.clone(), "..2026_10_18/settings.json", "{}");
        fixture.put(source.clone(), "..data/settings.json", "{}");
        fixture.put(source.clone(), "settings.json", r#"{"model":"opus"}"#);
        fixture.put(source.clone(), ".credentials.json", r#"{"token":"a"}"#);
        fixture.put(source.clone(), "skills/review/SKILL.md", "review");
        fixture.merge();

        assert_eq!(fixture.read("settings.json"), r#"{"model":"opus"}"#);
        assert_eq!(fixture.read(".credentials.json"), r#"{"token":"a"}"#);
        assert_eq!(fixture.read("skills/review/SKILL.md"), "review");
        assert!(!fixture.work().join("..data").exists());
        assert!(!fixture.work().join("..2026_10_18").exists());
        let mode = fs::metadata(fixture.work().join(".credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "only the owner reads the login");
    }

    #[test]
    fn a_symlinked_secret_volume_is_read_through_its_links() {
        let fixture = Fixture::new();
        let source = fixture.source();
        fixture.put(
            source.clone(),
            "..2026_10_18/skills/review/SKILL.md",
            "review",
        );
        fixture.put(source.clone(), "..2026_10_18/settings.json", "{}");
        std::os::unix::fs::symlink("..2026_10_18", source.join("..data")).unwrap();
        std::os::unix::fs::symlink("..data/skills", source.join("skills")).unwrap();
        std::os::unix::fs::symlink("..data/settings.json", source.join("settings.json")).unwrap();
        fixture.merge();

        assert_eq!(fixture.read("skills/review/SKILL.md"), "review");
        assert_eq!(fixture.read("settings.json"), "{}");
        assert!(!fs::symlink_metadata(fixture.work().join("skills"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_local_change_survives_a_restart() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), "CLAUDE.md", "shared");
        fixture.merge();
        fixture.put(fixture.work(), "CLAUDE.md", "local");
        fixture.merge();
        assert_eq!(fixture.read("CLAUDE.md"), "local");
    }

    #[test]
    fn a_source_change_reaches_the_harness() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), ".credentials.json", r#"{"token":"old"}"#);
        fixture.put(fixture.source(), "skills/old/SKILL.md", "old");
        fixture.merge();
        fixture.put(fixture.source(), ".credentials.json", r#"{"token":"new"}"#);
        fs::remove_dir_all(fixture.source().join("skills/old")).unwrap();
        fixture.merge();
        assert_eq!(fixture.read(".credentials.json"), r#"{"token":"new"}"#);
        assert!(
            !fixture.work().join("skills/old/SKILL.md").exists(),
            "a file the source dropped and the harness kept as it was goes too"
        );
    }

    #[test]
    fn settings_changed_on_both_sides_merge_keys_and_unite_rules() {
        let fixture = Fixture::new();
        let base = json!({
            "model": "opus",
            "permissions": {"allow": ["Read", "Bash(ls:*)"], "deny": []}
        });
        fixture.put(fixture.source(), "settings.json", &base.to_string());
        fixture.merge();
        let local = json!({
            "model": "sonnet",
            "theme": "dark",
            "permissions": {"allow": ["Read", "Bash(ls:*)", "Bash(cargo test:*)"], "deny": []}
        });
        fixture.put(fixture.work(), "settings.json", &local.to_string());
        let source = json!({
            "model": "fable",
            "permissions": {"allow": ["Read", "WebFetch"], "deny": ["Bash(rm:*)"]}
        });
        fixture.put(fixture.source(), "settings.json", &source.to_string());
        fixture.merge();

        assert_eq!(
            fixture.read_json("settings.json"),
            json!({
                "model": "fable",
                "theme": "dark",
                "permissions": {
                    "allow": ["Read", "Bash(cargo test:*)", "WebFetch"],
                    "deny": ["Bash(rm:*)"]
                }
            })
        );
    }

    #[test]
    fn credentials_changed_on_both_sides_take_the_sources() {
        let fixture = Fixture::new();
        fixture.put(
            fixture.source(),
            ".credentials.json",
            r#"{"a":"base","r":"base"}"#,
        );
        fixture.merge();
        fixture.put(
            fixture.work(),
            ".credentials.json",
            r#"{"a":"local","r":"local"}"#,
        );
        fixture.put(
            fixture.source(),
            ".credentials.json",
            r#"{"a":"source","r":"base"}"#,
        );
        fixture.merge();
        assert_eq!(
            fixture.read(".credentials.json"),
            r#"{"a":"source","r":"base"}"#
        );
    }

    #[test]
    fn other_files_changed_on_both_sides_take_the_sources() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), "CLAUDE.md", "base");
        fixture.merge();
        fixture.put(fixture.work(), "CLAUDE.md", "local");
        fixture.put(fixture.source(), "CLAUDE.md", "source");
        fixture.merge();
        assert_eq!(fixture.read("CLAUDE.md"), "source");
    }

    #[test]
    fn sessions_and_history_are_left_alone() {
        let fixture = Fixture::new();
        fixture.put(fixture.source(), "settings.json", "{}");
        fixture.merge();
        fixture.put(fixture.work(), "projects/-work/session.jsonl", "session");
        fixture.put(fixture.work(), "history.jsonl", "history");
        fixture.put(fixture.source(), "settings.json", r#"{"model":"opus"}"#);
        fixture.merge();
        assert_eq!(fixture.read("projects/-work/session.jsonl"), "session");
        assert_eq!(fixture.read("history.jsonl"), "history");
    }

    #[test]
    fn json_merge_respects_removals_on_either_side() {
        let base = json!({"keep": 1, "drop_local": 2, "drop_source": 3, "list": ["a", "b", "c"]});
        let local = json!({"keep": 1, "drop_source": 3, "list": ["a", "c", "d"]});
        let source = json!({"keep": 1, "drop_local": 2, "list": ["a", "b", "e"]});
        assert_eq!(
            merge_json(Some(&base), Some(&local), Some(&source)),
            Some(json!({"keep": 1, "list": ["a", "d", "e"]}))
        );
    }
}
