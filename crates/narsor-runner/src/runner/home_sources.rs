//! Read-only configuration sources copied into the private HOME (KTD11).
//!
//! A backend can hand the runner any number of read-only *home sources* —
//! a file or a directory, e.g. a mounted `skills/` directory or a
//! `.gitconfig` — each with a target relative to the private HOME. Unlike
//! the Claude profile ([`super::profile`]), which is merged so the
//! harness's own changes survive, a home source is plain configuration: at
//! every start its target is overwritten from the source. A file target is
//! replaced; a directory target is replaced as a whole, so a file removed
//! from the source is gone from HOME too.
//!
//! Every source is checked before anything is written: the target must stay
//! inside HOME, and a runner that keeps the host's HOME refuses home sources
//! rather than write into it, like `file` secret delivery. Secrets never use
//! this path; they stay links to their mounted files ([`super::secrets`]).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use super::profile::{create_private_dir, read_tree, remove_tree, replace_tree, write_private};
use super::{HomeMode, RunnerError};

/// One read-only source and where it goes under the private HOME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeSource {
    /// The read-only file or directory.
    pub source: PathBuf,
    /// The target, relative to the private HOME.
    pub target: PathBuf,
}

impl FromStr for HomeSource {
    type Err = String;

    /// Parses `<source>=<target>`. The target is after the last `=`, so a
    /// source path may itself hold one.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((source, target)) = value.rsplit_once('=') else {
            return Err(format!(
                "'{value}' is not <source>=<target relative to HOME>"
            ));
        };
        if source.is_empty() {
            return Err(format!("'{value}' names no source"));
        }
        let target = PathBuf::from(target);
        check_target(&target)?;
        Ok(Self {
            source: source.into(),
            target,
        })
    }
}

/// A target is a non-empty relative path made of plain names: no root, no
/// `..`, so it cannot leave HOME by its spelling.
fn check_target(target: &Path) -> Result<(), String> {
    let plain = target
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if target.as_os_str().is_empty() || !plain {
        return Err(format!(
            "the home source target '{}' must be a relative path inside HOME, without '..'",
            target.display()
        ));
    }
    Ok(())
}

/// Overwrites the target of every source under `private_home` from its
/// source. Checks every source before it writes anything.
pub fn prepare_home_sources(
    sources: &[HomeSource],
    mode: HomeMode,
    private_home: &Path,
) -> Result<(), RunnerError> {
    if sources.is_empty() {
        return Ok(());
    }
    if mode == HomeMode::Host {
        return Err(RunnerError::HomeSource(
            "a home source needs the private HOME (--home private)".to_string(),
        ));
    }
    for source in sources {
        check(source, private_home)?;
    }
    for source in sources {
        copy(source, &private_home.join(&source.target))?;
    }
    Ok(())
}

/// Refuses a source that is neither a file nor a directory, and a target
/// that leaves HOME by its spelling or through a symbolic link a tool left
/// on the way.
fn check(source: &HomeSource, home: &Path) -> Result<(), RunnerError> {
    check_target(&source.target).map_err(RunnerError::HomeSource)?;
    match fs::metadata(&source.source) {
        Ok(metadata) if metadata.is_dir() || metadata.is_file() => {}
        _ => {
            return Err(RunnerError::HomeSource(format!(
                "the home source '{}' is not a file or a directory",
                source.source.display()
            )))
        }
    }
    let mut ancestor = home.to_path_buf();
    let parents = source
        .target
        .parent()
        .into_iter()
        .flat_map(Path::components);
    for component in parents {
        ancestor.push(component);
        let is_link =
            fs::symlink_metadata(&ancestor).is_ok_and(|metadata| metadata.file_type().is_symlink());
        if is_link {
            return Err(RunnerError::HomeSource(format!(
                "the home source target '{}' passes through the symbolic link '{}'",
                source.target.display(),
                ancestor.display()
            )));
        }
    }
    Ok(())
}

/// Replaces `target` with the source's current content.
fn copy(source: &HomeSource, target: &Path) -> Result<(), RunnerError> {
    let metadata =
        fs::metadata(&source.source).map_err(|error| RunnerError::io(&source.source, error))?;
    let existing = fs::symlink_metadata(target).ok();
    if metadata.is_dir() {
        let files = read_tree(&source.source)?;
        if existing.is_some_and(|existing| !existing.is_dir()) {
            fs::remove_file(target).map_err(|error| RunnerError::io(target, error))?;
        }
        if let Some(parent) = target.parent() {
            create_private_dir(parent)?;
        }
        replace_tree(target, &files)
    } else {
        let bytes =
            fs::read(&source.source).map_err(|error| RunnerError::io(&source.source, error))?;
        if existing.is_some_and(|existing| existing.is_dir()) {
            remove_tree(target)?;
        }
        let executable =
            std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o111 != 0;
        let root = target.parent().unwrap_or(target);
        write_private(root, target, &bytes, executable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("narsor-home-sources-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("sources")).unwrap();
            Self { root }
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn put(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }

        fn source(&self, source: &str, target: &str) -> HomeSource {
            HomeSource {
                source: self.root.join(source),
                target: target.into(),
            }
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.home().join(relative)).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn every_start_overwrites_home_from_its_sources() {
        let fixture = Fixture::new();
        fixture.put("sources/skills/review/SKILL.md", "review v1");
        fixture.put("sources/skills/old/SKILL.md", "old");
        fixture.put("sources/skills/..data/ignored", "volume");
        fixture.put("sources/gitconfig", "[user]\n\tname = one\n");
        let sources = [
            fixture.source("sources/skills", ".claude/skills"),
            fixture.source("sources/gitconfig", ".gitconfig"),
        ];

        prepare_home_sources(&sources, HomeMode::Private, &fixture.home()).unwrap();
        assert_eq!(fixture.read(".claude/skills/review/SKILL.md"), "review v1");
        assert_eq!(fixture.read(".claude/skills/old/SKILL.md"), "old");
        assert_eq!(fixture.read(".gitconfig"), "[user]\n\tname = one\n");
        assert!(!fixture.home().join(".claude/skills/..data").exists());

        // The harness changes its copy, the source changes and drops a file.
        fixture.put("home/.gitconfig", "local edit");
        fixture.put("home/.claude/skills/review/SKILL.md", "local edit");
        fixture.put("sources/skills/review/SKILL.md", "review v2");
        fs::remove_dir_all(fixture.root.join("sources/skills/old")).unwrap();
        fixture.put("sources/gitconfig", "[user]\n\tname = two\n");

        prepare_home_sources(&sources, HomeMode::Private, &fixture.home()).unwrap();
        assert_eq!(fixture.read(".claude/skills/review/SKILL.md"), "review v2");
        assert!(!fixture.home().join(".claude/skills/old").exists());
        assert_eq!(fixture.read(".gitconfig"), "[user]\n\tname = two\n");
    }

    #[test]
    fn a_source_that_could_escape_home_is_refused_before_any_write() {
        let fixture = Fixture::new();
        fixture.put("sources/gitconfig", "config");
        let good = fixture.source("sources/gitconfig", ".gitconfig");
        let cases = [
            (
                "escaping target",
                fixture.source("sources/gitconfig", "../x"),
                HomeMode::Private,
                "inside HOME",
            ),
            (
                "absolute target",
                fixture.source("sources/gitconfig", "/etc/x"),
                HomeMode::Private,
                "inside HOME",
            ),
            ("host HOME", good.clone(), HomeMode::Host, "--home private"),
            (
                "missing source",
                fixture.source("sources/absent", ".absent"),
                HomeMode::Private,
                "not a file",
            ),
            (
                "symbolic link in HOME",
                fixture.source("sources/gitconfig", "link/x"),
                HomeMode::Private,
                "symbolic link",
            ),
        ];
        let outside = fixture.root.join("outside");
        for (case, bad, mode, expected) in cases {
            if bad.target.starts_with("link") {
                fs::create_dir_all(&outside).unwrap();
                fs::create_dir_all(fixture.home()).unwrap();
                std::os::unix::fs::symlink(&outside, fixture.home().join("link")).unwrap();
            }

            let error =
                prepare_home_sources(&[good.clone(), bad], mode, &fixture.home()).expect_err(case);

            assert!(error.to_string().contains(expected), "{case}: {error}");
            let written: Vec<_> = fs::read_dir(fixture.home())
                .into_iter()
                .flatten()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name != "link")
                .collect();
            assert!(written.is_empty(), "{case}: nothing was written");
            assert!(
                fs::read_dir(&outside).map_or(true, |mut dir| dir.next().is_none()),
                "{case}: nothing was written outside HOME"
            );
        }
    }

    #[test]
    fn the_argument_names_a_source_and_a_target() {
        assert_eq!(
            "/mnt/a=b/skills=.claude/skills".parse::<HomeSource>(),
            Ok(HomeSource {
                source: "/mnt/a=b/skills".into(),
                target: ".claude/skills".into(),
            })
        );
        for invalid in ["/mnt/skills", "=.gitconfig", "/mnt/x=", "/mnt/x=../y"] {
            assert!(invalid.parse::<HomeSource>().is_err(), "{invalid}");
        }
    }
}
