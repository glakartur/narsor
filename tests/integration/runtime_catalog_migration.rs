//! `narsor-migrate runtime-catalog`: the move of an old runtime catalogue
//! (`{catalog}/runtime.yaml`, `{catalog}/components/`) into
//! `{catalog}/runtime/`, on temp catalogues.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use narsor_migrate::runtime_catalog::{self, Failure, Outcome, RuntimeCatalogMove};
use narsor_migrate::{EXIT_REFUSED, EXIT_RUNNING};
use narsor_shared::config::resolve::{resolve_with, Environment, Flags};
use narsor_shared::config::{ConfigCascade, Scope};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("narsor-runtime-move-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        Self(fs::canonicalize(&dir).unwrap())
    }

    /// The catalogue of an installation rooted here.
    fn catalog(&self) -> PathBuf {
        self.0.join("catalog")
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.catalog().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Every file below the catalogue with its text, by relative path.
    fn tree(&self) -> BTreeMap<String, String> {
        fn walk(dir: &Path, base: &Path, files: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, base, files);
                } else {
                    let relative = path.strip_prefix(base).unwrap().display().to_string();
                    files.insert(relative, fs::read_to_string(&path).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(&self.catalog(), &self.catalog(), &mut files);
        files
    }

    /// The move for the installation `NARSOR_GLOBAL__ROOT` names, with
    /// `server.catalog` below it.
    fn target(&self) -> RuntimeCatalogMove {
        let environment: Environment = [
            ("NARSOR_GLOBAL__ROOT", self.0.display().to_string()),
            (
                "NARSOR_SERVER__CATALOG",
                self.catalog().display().to_string(),
            ),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
        RuntimeCatalogMove::of(&resolve_with(
            &Flags::default(),
            &environment,
            &ConfigCascade::with_dirs(None, None, None),
            Scope::SERVER,
        ))
    }

    fn run(&self, dry_run: bool) -> (Result<Outcome, Failure>, String) {
        let mut out = Vec::new();
        let result = runtime_catalog::run(&self.target(), dry_run, &mut out);
        (result, String::from_utf8(out).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SCRIPT_RUNTIME: &str = "task_image:\n  base: debian:trixie-slim\n  components:\n    \
     - { name: glab, install: components/glab/install.sh }\n";

fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect()
}

#[test]
fn an_old_layout_moves_into_the_runtime_directory_with_the_same_bytes() {
    let inline = "task_image:\n  base: debian:trixie-slim\n  components:\n    \
                  - { name: one, install: { inline: \"echo one\" } }\n";
    // (name, files before, files after)
    let cases = [
        (
            "file and scripts",
            vec![
                ("runtime.yaml", SCRIPT_RUNTIME),
                ("components/glab/install.sh", "echo glab\n"),
            ],
            vec![
                ("runtime/runtime.yaml", SCRIPT_RUNTIME),
                ("runtime/components/glab/install.sh", "echo glab\n"),
            ],
        ),
        (
            "inline scripts only",
            vec![("runtime.yaml", inline)],
            vec![("runtime/runtime.yaml", inline)],
        ),
        (
            "rerun after a crash between the renames",
            vec![
                ("runtime.yaml", SCRIPT_RUNTIME),
                ("runtime/components/glab/install.sh", "echo glab\n"),
            ],
            vec![
                ("runtime/runtime.yaml", SCRIPT_RUNTIME),
                ("runtime/components/glab/install.sh", "echo glab\n"),
            ],
        ),
    ];
    for (name, before, after) in cases {
        let dir = TempDir::new();
        for (path, text) in &before {
            dir.write(path, text);
        }

        let (result, out) = dir.run(false);

        assert_eq!(
            result,
            Ok(Outcome::Moved { warnings: vec![] }),
            "{name}: {out}"
        );
        assert_eq!(dir.tree(), files(&after), "{name}");
        assert!(!dir.catalog().join("components").exists(), "{name}");
    }
}

#[test]
fn a_script_outside_components_moves_with_a_warning_that_names_it() {
    let dir = TempDir::new();
    let runtime = "task_image:\n  base: debian:trixie-slim\n  components:\n    \
                   - { name: x, install: tools/x.sh, version: components/x/version.sh }\n";
    dir.write("runtime.yaml", runtime);
    dir.write("tools/x.sh", "echo x\n");

    let (result, out) = dir.run(false);

    let Ok(Outcome::Moved { warnings }) = result else {
        panic!("the layout moves: {result:?}\n{out}");
    };
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("task_image.components[0].install")
            && warnings[0].contains("tools/x.sh")
            && out.contains(&warnings[0]),
        "{out}"
    );
    assert_eq!(
        dir.tree(),
        files(&[
            ("runtime/runtime.yaml", runtime),
            ("tools/x.sh", "echo x\n")
        ])
    );
}

#[test]
fn nothing_changes_on_a_collision_a_running_program_or_a_dry_run() {
    let old_layout = [
        ("runtime.yaml", SCRIPT_RUNTIME),
        ("components/glab/install.sh", "echo glab\n"),
    ];
    type Arrange = Box<dyn Fn(&TempDir) -> Option<fs::File>>;
    // (name, extra arrangement, dry run, expected exit or None for Ok, text in the output)
    let cases: Vec<(&str, Arrange, bool, Option<i32>, &str)> = vec![
        (
            "both runtime files",
            Box::new(|dir: &TempDir| {
                dir.write("runtime/runtime.yaml", "task_image:\n  base: alpine:3\n");
                None
            }),
            false,
            Some(EXIT_REFUSED),
            "runtime/runtime.yaml",
        ),
        (
            "both components directories",
            Box::new(|dir: &TempDir| {
                dir.write("runtime/components/other.sh", "echo other\n");
                None
            }),
            false,
            Some(EXIT_REFUSED),
            "runtime/components",
        ),
        (
            "a running server",
            Box::new(|dir: &TempDir| {
                let path = dir.0.join("state/narsor-server/daemon.instance.lock");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, "").unwrap();
                let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
                use std::os::fd::AsRawFd as _;
                assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
                Some(file)
            }),
            false,
            Some(EXIT_RUNNING),
            "daemon.instance.lock",
        ),
        (
            "dry run",
            Box::new(|_: &TempDir| None),
            true,
            None,
            "Would move",
        ),
    ];
    for (name, arrange, dry_run, exit, text) in cases {
        let dir = TempDir::new();
        for (path, content) in old_layout {
            dir.write(path, content);
        }
        let _held = arrange(&dir);
        let before = dir.tree();

        let (result, out) = dir.run(dry_run);

        match (exit, &result) {
            (None, Ok(Outcome::DryRun)) => assert!(out.contains(text), "{name}: {out}"),
            (Some(code), Err(failure)) => {
                assert_eq!(failure.exit_code(), code, "{name}: {failure}");
                let message = failure.to_string();
                assert!(message.contains(text), "{name}: {message}");
            }
            _ => panic!("{name}: {result:?}\n{out}"),
        }
        assert_eq!(dir.tree(), before, "{name}: nothing changed");
    }
}

#[test]
fn a_catalogue_without_an_old_runtime_file_has_nothing_to_move() {
    let dir = TempDir::new();
    dir.write("runtime/runtime.yaml", SCRIPT_RUNTIME);

    let (result, _) = dir.run(false);

    assert_eq!(result, Ok(Outcome::NothingToMove));
    assert_eq!(
        dir.tree(),
        files(&[("runtime/runtime.yaml", SCRIPT_RUNTIME)])
    );
}
