//! The part of the task workspace one step may touch (KTD5).
//!
//! [`StepScope::resolve`] turns a step's `workspace` section and the
//! execution's [`Workspace`] into the directory the step starts in and the
//! repositories it may write and only read. Both the agent and the script
//! executor start from the same scope, and the prompt block and the harness
//! adapters are fed from it.

use std::path::PathBuf;

use crate::domain::{StepWorkspace, Workspace, WORKSPACE_ROOT};

/// One repository of a step, by name and absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRepository {
    pub name: String,
    pub path: PathBuf,
}

/// Where a step starts and what it may write or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepScope {
    /// The directory the step's process starts in.
    pub start_dir: PathBuf,

    /// Repositories the step may change.
    pub write: Vec<ScopedRepository>,

    /// Repositories the step may only read.
    pub read: Vec<ScopedRepository>,

    /// Whether the workflow has more than one repository, so the prompt
    /// has to say which ones the step sees.
    pub multi_repository: bool,
}

impl StepScope {
    /// Resolves `section` (`None` for a step without one) against `workspace`.
    ///
    /// Without a section the step gets every repository, the primary one
    /// writable and starting there (R9); with one, only `root` and the
    /// repositories it names (R10), starting at `root`, which is writable
    /// unless `read` lists it. A name the workspace does not have is
    /// skipped: the validator rejects it before a task runs.
    pub fn resolve(section: Option<&StepWorkspace>, workspace: &Workspace) -> Self {
        let entries = workspace.repository_entries();
        let find = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| ScopedRepository {
                    name: entry.name.clone(),
                    path: PathBuf::from(&entry.path),
                })
        };
        let multi_repository = entries.len() > 1;

        let Some(section) = section else {
            let (primary, others): (Vec<_>, Vec<_>) =
                entries.iter().partition(|entry| entry.primary);
            let scoped = |entry: &&crate::domain::WorkspaceRepository| ScopedRepository {
                name: entry.name.clone(),
                path: PathBuf::from(&entry.path),
            };
            return Self {
                start_dir: PathBuf::from(&workspace.path),
                write: primary.iter().map(scoped).collect(),
                read: others.iter().map(scoped).collect(),
                multi_repository,
            };
        };

        let mut write: Vec<_> = Vec::new();
        if section.root_is_writable() {
            write.extend(find(&section.root));
        }
        for repository in section.write.iter().filter_map(|name| find(name)) {
            if !write.contains(&repository) {
                write.push(repository);
            }
        }
        let read: Vec<_> = section.read.iter().filter_map(|name| find(name)).collect();
        let start_dir = if section.root == WORKSPACE_ROOT {
            Some(PathBuf::from(workspace.root_dir()))
        } else {
            find(&section.root).map(|repository| repository.path)
        }
        .unwrap_or_else(|| PathBuf::from(&workspace.path));

        Self {
            start_dir,
            write,
            read,
            multi_repository,
        }
    }

    /// The same scope with every repository read-only, for a run that must
    /// not change anything (a reprompt, the conductor).
    pub fn read_only(mut self) -> Self {
        let write = std::mem::take(&mut self.write);
        self.read.splice(0..0, write);
        self
    }

    /// Absolute paths of the writable repositories.
    pub fn write_dirs(&self) -> Vec<PathBuf> {
        self.write.iter().map(|r| r.path.clone()).collect()
    }

    /// Absolute paths of the read-only repositories.
    pub fn read_dirs(&self) -> Vec<PathBuf> {
        self.read.iter().map(|r| r.path.clone()).collect()
    }

    /// The prompt block naming each repository, its path and its mode (R13).
    /// Empty when the workflow has a single repository.
    pub fn prompt_block(&self) -> String {
        if !self.multi_repository {
            return String::new();
        }
        let mut block = String::from("\n\nRepositories available to this step:\n");
        for (repositories, mode) in [(&self.write, "write"), (&self.read, "read-only")] {
            for repository in repositories {
                block.push_str(&format!(
                    "- {} ({}): {}\n",
                    repository.name,
                    repository.path.display(),
                    mode
                ));
            }
        }
        block.push_str("Do not modify the read-only repositories.");
        block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::WorkspaceRepository;

    fn workspace(names: &[&str]) -> Workspace {
        let mut workspace = Workspace::local_folder(format!("/task/workspace/{}", names[0]));
        workspace.root = Some("/task/workspace".to_string());
        workspace.repositories = names
            .iter()
            .enumerate()
            .map(|(index, name)| WorkspaceRepository {
                name: name.to_string(),
                path: format!("/task/workspace/{name}"),
                primary: index == 0,
                branch: None,
                repository: None,
                base_commit: None,
            })
            .collect();
        workspace
    }

    fn names(repositories: &[ScopedRepository]) -> Vec<&str> {
        repositories.iter().map(|r| r.name.as_str()).collect()
    }

    fn section(root: &str, write: &[&str], read: &[&str]) -> StepWorkspace {
        StepWorkspace {
            root: root.to_string(),
            write: write.iter().map(ToString::to_string).collect(),
            read: read.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn root_dot_starts_in_the_parent_and_lists_only_named_repositories() {
        let workspace = workspace(&["a", "b", "c", "d", "e"]);
        let scope = StepScope::resolve(Some(&section(".", &["d"], &["a", "b", "c"])), &workspace);
        assert_eq!(scope.start_dir, PathBuf::from("/task/workspace"));
        assert_eq!(names(&scope.write), ["d"]);
        assert_eq!(names(&scope.read), ["a", "b", "c"]);
    }

    #[test]
    fn a_step_without_a_section_writes_the_primary_and_reads_the_rest() {
        let workspace = workspace(&["code", "docs"]);
        let scope = StepScope::resolve(None, &workspace);
        assert_eq!(scope.start_dir, PathBuf::from("/task/workspace/code"));
        assert_eq!(names(&scope.write), ["code"]);
        assert_eq!(names(&scope.read), ["docs"]);
    }

    #[test]
    fn a_docs_step_starts_in_docs_and_reads_code() {
        let workspace = workspace(&["code", "docs"]);
        let scope = StepScope::resolve(Some(&section("docs", &[], &["code"])), &workspace);
        assert_eq!(scope.start_dir, PathBuf::from("/task/workspace/docs"));
        let block = scope.prompt_block();
        assert!(block.contains("- code (/task/workspace/code): read-only"));
        assert!(block.contains("- docs (/task/workspace/docs): write"));
    }

    #[test]
    fn root_is_writable_unless_read_lists_it() {
        let workspace = workspace(&["a", "b", "c"]);
        let scope = StepScope::resolve(Some(&section("a", &["c"], &["b"])), &workspace);
        assert_eq!(names(&scope.write), ["a", "c"]);
        assert_eq!(names(&scope.read), ["b"]);

        let scope = StepScope::resolve(Some(&section("a", &["c"], &["a", "b"])), &workspace);
        assert_eq!(scope.start_dir, PathBuf::from("/task/workspace/a"));
        assert_eq!(names(&scope.write), ["c"]);
        assert_eq!(names(&scope.read), ["a", "b"]);
    }

    #[test]
    fn a_single_repository_has_no_prompt_block_and_starts_in_place() {
        let workspace = Workspace::local_folder("/task/workspace/app".to_string());
        let scope = StepScope::resolve(None, &workspace);
        assert_eq!(scope.start_dir, PathBuf::from("/task/workspace/app"));
        assert_eq!(scope.prompt_block(), "");
    }

    #[test]
    fn read_only_moves_every_repository_to_the_read_list() {
        let workspace = workspace(&["code", "docs"]);
        let scope = StepScope::resolve(None, &workspace).read_only();
        assert!(scope.write.is_empty());
        assert_eq!(names(&scope.read), ["code", "docs"]);
    }
}
