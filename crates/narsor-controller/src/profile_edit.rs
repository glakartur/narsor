//! Editing the Claude Code profile Secret in the controller (R15-R17, KTD7).
//!
//! The controller is the only writer of the profile Secret. An edit session
//! is independent of how the user reaches the harness's terminal:
//!
//! 1. [`EditSession::open`] writes the Secret's files into a private
//!    directory, each at the path its key is mapped to;
//! 2. the caller runs Claude Code with that directory as its
//!    `CLAUDE_CONFIG_DIR` and waits for the user to close it;
//! 3. [`EditSession::changes`] collects the profile files from the directory
//!    and compares them with the Secret;
//! 4. the caller writes [`ProfileChanges::updated`] to the Secret.
//!
//! Only profile files are collected: the mapped paths, plus what Claude Code
//! keeps as configuration (settings, login, `CLAUDE.md`, MCP, skills,
//! agents, commands, plugin registries). Session history, projects, todos,
//! shell snapshots and plugin caches are never part of the profile. A
//! profile file with no `key=path` mapping cannot be stored in the Secret
//! and is reported instead.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// Files at the top of Claude's configuration directory that belong to the
/// profile.
const PROFILE_FILES: [&str; 6] = [
    "settings.json",
    "settings.local.json",
    ".credentials.json",
    ".claude.json",
    ".mcp.json",
    "CLAUDE.md",
];

/// Paths under Claude's configuration directory whose files belong to the
/// profile: whole directories (ending in `/`) or single files.
const PROFILE_TREES: [&str; 7] = [
    "skills/",
    "agents/",
    "commands/",
    "output-styles/",
    "plugins/installed_plugins.json",
    "plugins/known_marketplaces.json",
    "plugins/config.json",
];

/// A profile Secret's files laid out as Claude's configuration directory.
#[derive(Debug)]
pub struct EditSession {
    root: PathBuf,
    /// `(Secret key, relative path)`, as configured.
    mappings: Vec<(String, String)>,
}

impl EditSession {
    /// Creates a private session directory under `parent` and writes every
    /// mapped key of `secret` at its path in [`Self::config_dir`]. A mapped
    /// key the Secret does not have yet is simply absent.
    pub fn open(
        parent: &Path,
        mappings: Vec<(String, String)>,
        secret: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, String> {
        let root = parent.join(format!("narsor-profile-edit-{}", uuid::Uuid::new_v4()));
        create_private_dir(&root)?;
        let session = Self { root, mappings };
        create_private_dir(&session.config_dir())?;
        create_private_dir(&session.home_dir())?;
        for (key, path) in &session.mappings {
            if let Some(bytes) = secret.get(key) {
                write_private(&session.config_dir().join(path), bytes)?;
            }
        }
        Ok(session)
    }

    /// The directory to run Claude Code on (`CLAUDE_CONFIG_DIR`).
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// A private, empty `HOME` for the harness, so that nothing it writes
    /// outside its configuration directory leaves the session.
    pub fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    /// What saving the session would change in a Secret holding `current`.
    pub fn changes(&self, current: &BTreeMap<String, Vec<u8>>) -> Result<ProfileChanges, String> {
        let collected = collect_profile(&self.config_dir(), &self.mappings)?;
        let mut changes = ProfileChanges::default();
        let mut mapped = BTreeMap::new();
        for (key, path) in &self.mappings {
            mapped.insert(path.as_str(), key.as_str());
            match collected.get(Path::new(path)) {
                Some(bytes) if current.get(key) != Some(bytes) => {
                    changes.updated.insert(key.clone(), bytes.clone());
                }
                Some(_) => {}
                None if current.contains_key(key) => changes.missing.push(path.clone()),
                None => {}
            }
        }
        changes.unmapped = collected
            .keys()
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !mapped.contains_key(path.as_str()))
            .collect();
        Ok(changes)
    }
}

impl Drop for EditSession {
    /// The session holds a login: it never outlives its use.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The outcome of an edit session, compared with the Secret.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProfileChanges {
    /// Secret keys whose files changed, with their new content.
    pub updated: BTreeMap<String, Vec<u8>>,
    /// Mapped paths the user removed. The Secret keeps their keys: a task
    /// Pod mounts every mapped key, and a missing one would keep it from
    /// starting.
    pub missing: Vec<String>,
    /// Profile files with no `key=path` mapping, which the Secret cannot
    /// hold until one is configured.
    pub unmapped: Vec<String>,
}

impl ProfileChanges {
    /// Whether saving would change the Secret.
    pub fn is_empty(&self) -> bool {
        self.updated.is_empty()
    }
}

/// Every profile file under `dir`: the mapped paths and the paths of
/// [`PROFILE_FILES`] and [`PROFILE_TREES`], by path relative to `dir`.
pub fn collect_profile(
    dir: &Path,
    mappings: &[(String, String)],
) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    files.retain(|relative, _| {
        let relative = relative.to_string_lossy();
        is_profile_path(&relative) || mappings.iter().any(|(_, path)| *path == relative)
    });
    Ok(files)
}

fn is_profile_path(relative: &str) -> bool {
    PROFILE_FILES.contains(&relative)
        || PROFILE_TREES
            .iter()
            .any(|tree| match tree.strip_suffix('/') {
                Some(_) => relative.starts_with(tree),
                None => relative == *tree,
            })
}

/// Regular files only; a symbolic link is not followed out of the session.
fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if kind.is_dir() {
            walk(root, &path, files)?;
        } else if kind.is_file() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let relative = path
                .strip_prefix(root)
                .expect("a walked path lies under its root")
                .to_path_buf();
            files.insert(relative, bytes);
        }
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mappings() -> Vec<(String, String)> {
        [
            ("settings", "settings.json"),
            ("auth", ".credentials.json"),
            ("review-skill", "skills/review/SKILL.md"),
            ("hook", "hooks/check.sh"),
        ]
        .into_iter()
        .map(|(key, path)| (key.to_string(), path.to_string()))
        .collect()
    }

    fn secret() -> BTreeMap<String, Vec<u8>> {
        [
            ("settings", r#"{"model":"opus"}"#),
            ("auth", r#"{"token":"a"}"#),
            ("review-skill", "review"),
            ("hook", "#!/bin/sh"),
            ("unrelated", "kept by the Secret"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
        .collect()
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("narsor-edit-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_session_lays_the_secret_out_as_the_configuration_directory() {
        let parent = scratch();
        let session = EditSession::open(&parent, mappings(), &secret()).unwrap();
        let config = session.config_dir();
        assert_eq!(
            fs::read_to_string(config.join("settings.json")).unwrap(),
            r#"{"model":"opus"}"#
        );
        assert_eq!(
            fs::read_to_string(config.join("skills/review/SKILL.md")).unwrap(),
            "review"
        );
        assert!(!config.join("unrelated").exists(), "only mapped keys");
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(config.join(".credentials.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            session.changes(&secret()).unwrap().is_empty(),
            "nothing edited"
        );

        let root = session.root.clone();
        drop(session);
        assert!(!root.exists(), "the login does not outlive the session");
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn edited_and_added_files_are_saved_and_history_is_not() {
        let parent = scratch();
        let mut mapped = mappings();
        mapped.push(("lint-skill".into(), "skills/lint/SKILL.md".into()));
        let session = EditSession::open(&parent, mapped, &secret()).unwrap();
        let config = session.config_dir();
        fs::write(config.join("settings.json"), r#"{"model":"fable"}"#).unwrap();
        fs::create_dir_all(config.join("skills/lint")).unwrap();
        fs::write(config.join("skills/lint/SKILL.md"), "lint").unwrap();
        fs::create_dir_all(config.join("skills/draft")).unwrap();
        fs::write(config.join("skills/draft/SKILL.md"), "draft").unwrap();
        fs::create_dir_all(config.join("projects/-work")).unwrap();
        fs::write(config.join("projects/-work/session.jsonl"), "session").unwrap();
        fs::write(config.join("history.jsonl"), "history").unwrap();
        fs::create_dir_all(config.join("plugins/cache/tool")).unwrap();
        fs::write(config.join("plugins/cache/tool/index.js"), "cache").unwrap();
        fs::remove_file(config.join("hooks/check.sh")).unwrap();

        let changes = session.changes(&secret()).unwrap();
        assert_eq!(
            changes.updated,
            BTreeMap::from([
                ("lint-skill".to_string(), b"lint".to_vec()),
                ("settings".to_string(), br#"{"model":"fable"}"#.to_vec()),
            ])
        );
        assert_eq!(changes.missing, vec!["hooks/check.sh".to_string()]);
        assert_eq!(changes.unmapped, vec!["skills/draft/SKILL.md".to_string()]);
        drop(session);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn profile_paths_are_configuration_only() {
        for path in [
            "settings.json",
            ".claude.json",
            "CLAUDE.md",
            "skills/review/SKILL.md",
            "agents/reviewer.md",
            "plugins/installed_plugins.json",
        ] {
            assert!(is_profile_path(path), "{path}");
        }
        for path in [
            "history.jsonl",
            "projects/-work/session.jsonl",
            "todos/1.json",
            "shell-snapshots/a.sh",
            "plugins/cache/x/index.js",
            "plugins/marketplaces/x/README.md",
            "skillsets/x",
        ] {
            assert!(!is_profile_path(path), "{path}");
        }
    }
}
