//! Tests for the built-in workflow and the search path that reaches it.

use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::domain::ActionConfig;
use narsor_engine::workflow::{load_from_str, validate_source};
use narsor_engine::workflows::{
    builtin, builtin_names, WorkflowOrigin, WorkflowResolver, BUILTIN_WORKFLOWS,
};

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-workflows-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::create_dir_all(&path).expect("create directory under temp root");
        path
    }

    fn root(&self) -> PathBuf {
        self.dir("root")
    }

    fn root_workflows(&self) -> PathBuf {
        self.dir("root/workflows")
    }

    fn user_workflows(&self) -> PathBuf {
        self.dir("config/narsor/workflows")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn resolver(root: &TempRoot) -> WorkflowResolver {
    WorkflowResolver::with_user_dir(root.root(), Some(root.user_workflows()))
}

fn write_workflow(directory: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(directory).expect("create workflow directory");
    let path = directory.join(name);
    fs::write(&path, body).expect("write workflow");
    path
}

fn marked_definition(marker: &str) -> String {
    format!(
        "name: {marker}\n\
         version: 1\n\
         description: A stand-in definition named {marker}.\n\
         states:\n\
         \x20 done:\n\
         \x20   description: Nothing to do.\n\
         terminal_states:\n\
         \x20 - done\n"
    )
}

#[test]
fn the_template_is_the_only_builtin_and_demonstrates_engine_features_safely() {
    assert_eq!(builtin_names(), vec!["template"]);
    assert_eq!(BUILTIN_WORKFLOWS.len(), 1);
    let entry = builtin("template").expect("template is built in");
    let violations = validate_source(entry.source).expect("template parses");
    assert!(violations.is_empty(), "{violations:?}");

    let definition = load_from_str(entry.source).expect("template loads");
    assert_eq!(definition.name, "template");
    assert!(definition.display.is_some());
    assert_eq!(definition.inputs.len(), 1);
    assert_eq!(definition.globals.timeout_secs, Some(120));
    assert_eq!(
        definition
            .globals
            .retry_policy
            .as_ref()
            .map(|policy| policy.max_attempts),
        Some(2)
    );

    let demonstrate = &definition.states["demonstrate"];
    assert!(matches!(demonstrate.action, Some(ActionConfig::Script(_))));
    assert_eq!(demonstrate.variables[0].name, "example_value");
    assert_eq!(demonstrate.artifacts[0].name, "example_file");
    assert_eq!(
        demonstrate.choices[0].requires_variables,
        vec!["example_value"]
    );
    assert_eq!(
        demonstrate.choices[0].requires_artifacts,
        vec!["example_file"]
    );

    let review = &definition.states["review"];
    assert!(matches!(review.action, Some(ActionConfig::Human(_))));
    assert!(review.choices.iter().any(|choice| choice.label == "finish"));

    assert!(
        definition.states.values().all(|state| !matches!(
            state.action,
            Some(ActionConfig::Agent(_) | ActionConfig::Wait(_))
        )),
        "the template does not invoke an agent or an external event service"
    );

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows/template.yaml");
    assert_eq!(
        entry.source,
        fs::read_to_string(&path).expect("read template source"),
        "the embedded definition matches {}",
        path.display()
    );
}

#[test]
fn a_bare_name_and_a_suffixed_name_resolve_to_the_same_builtin() {
    let root = TempRoot::new();
    let resolver = resolver(&root);

    let bare = resolver.resolve("template").expect("bare name resolves");
    assert_eq!(bare.origin, WorkflowOrigin::BuiltIn);
    assert_eq!(bare.name, "template");
    assert_eq!(
        bare.source,
        builtin("template").expect("built-in exists").source
    );
    assert!(bare.path().is_none());

    for suffixed in ["template.yaml", "template.yml"] {
        assert_eq!(
            resolver.resolve(suffixed).expect("suffixed name resolves"),
            bare
        );
    }
}

#[test]
fn a_file_path_wins_over_a_builtin_of_the_same_name() {
    let root = TempRoot::new();
    let local = write_workflow(
        &root.dir("drafts"),
        "template.yaml",
        &marked_definition("from-the-file"),
    );

    let resolved = resolver(&root)
        .resolve(&local.display().to_string())
        .expect("file path resolves");

    assert_eq!(resolved.origin.label(), "file");
    assert_eq!(
        resolved.path(),
        Some(local.canonicalize().unwrap().as_path())
    );
    assert!(resolved.source.contains("from-the-file"));
}

#[test]
fn the_root_directory_shadows_the_builtin() {
    let root = TempRoot::new();
    write_workflow(
        &root.root_workflows(),
        "template.yaml",
        &marked_definition("from-the-root"),
    );

    let resolved = resolver(&root)
        .resolve("template")
        .expect("template resolves");

    assert_eq!(resolved.origin.label(), "root");
    assert!(resolved.source.contains("from-the-root"));
}

#[test]
fn the_user_directory_shadows_the_builtin_but_loses_to_the_root() {
    let root = TempRoot::new();
    write_workflow(
        &root.user_workflows(),
        "template.yaml",
        &marked_definition("from-the-user-directory"),
    );

    let resolved = resolver(&root)
        .resolve("template")
        .expect("template resolves");
    assert_eq!(resolved.origin.label(), "user");
    assert!(resolved.source.contains("from-the-user-directory"));

    write_workflow(
        &root.root_workflows(),
        "template.yaml",
        &marked_definition("from-the-root"),
    );

    let resolved = resolver(&root)
        .resolve("template")
        .expect("template still resolves");
    assert_eq!(resolved.origin.label(), "root");
    assert!(resolved.source.contains("from-the-root"));
}

#[test]
fn an_unknown_name_lists_the_available_builtin() {
    let root = TempRoot::new();
    let error = resolver(&root)
        .resolve("no-such-workflow")
        .expect_err("unknown workflow does not resolve");
    let message = error.to_string();

    assert!(message.contains("no-such-workflow"));
    assert!(message.contains(&root.root_workflows().display().to_string()));
    assert!(message.contains(&root.user_workflows().display().to_string()));
    assert!(message.contains("template"));
}

#[test]
fn the_listing_reports_a_root_override_in_the_builtin_row() {
    let root = TempRoot::new();
    let resolver = resolver(&root);

    let entries = resolver.list();
    assert_eq!(entries.len(), 1, "only the template is built in");
    assert_eq!(entries[0].name, "template");
    assert_eq!(entries[0].origin, WorkflowOrigin::BuiltIn);
    assert!(!entries[0].is_override());
    assert!(entries[0].description.is_some());

    write_workflow(
        &root.root_workflows(),
        "template.yaml",
        &marked_definition("from-the-root"),
    );

    let entries = resolver.list();
    assert_eq!(entries.len(), 1, "the override is not a second row");
    assert_eq!(entries[0].name, "template");
    assert_eq!(entries[0].origin.label(), "root");
    assert!(entries[0].is_override());
    assert_eq!(entries[0].shadow_labels(), "built-in");
}
