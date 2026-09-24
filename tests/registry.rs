//! Integration tests for the repository registry.
//!
//! Everything runs against a real temporary root, because the value of this
//! layer is what actually lands on disk: one JSON document, written through the
//! atomic writer, that a missing file reads back as empty.

use narsor::registry::{RegistryError, RepositoryEntry, RepositoryRegistry};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory that deletes itself when it goes out of scope.
///
/// Stands in for the `tempfile` crate, which is not a dependency of this crate.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-registry-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn registry(&self) -> RepositoryRegistry {
        RepositoryRegistry::new(&self.path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// An entry with every optional field filled in, so a round-trip can lose one.
fn full_entry() -> RepositoryEntry {
    RepositoryEntry::new("acme-api", "https://github.com/acme/api.git")
        .with_name("ACME API")
        .with_provider("github")
        .with_default_branch("main")
        .with_description("The public API service.")
}

// ---------------------------------------------------------------------------
// Reading and writing
// ---------------------------------------------------------------------------

#[test]
fn an_added_entry_reads_back_with_every_field_intact() {
    let root = TempRoot::new();
    let registry = root.registry();

    registry.add(full_entry()).expect("add the entry");

    let stored = registry
        .get("acme-api")
        .expect("read the registry")
        .expect("the entry is registered");
    assert_eq!(stored, full_entry());
}

#[test]
fn a_missing_file_is_an_empty_registry_and_not_an_error() {
    let root = TempRoot::new();
    let registry = root.registry();

    assert!(!registry.path().exists(), "nothing was written yet");
    assert!(registry.list().expect("list an empty registry").is_empty());
    assert_eq!(registry.get("anything").expect("get from empty"), None);
}

#[test]
fn list_is_ordered_by_id_whatever_order_entries_were_added_in() {
    let root = TempRoot::new();
    let registry = root.registry();

    for id in ["zeta", "alpha", "mu"] {
        registry
            .add(RepositoryEntry::new(
                id,
                format!("https://example.com/{id}.git"),
            ))
            .expect("add the entry");
    }

    let ids: Vec<String> = registry
        .list()
        .expect("list")
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert_eq!(ids, ["alpha", "mu", "zeta"]);
}

#[test]
fn update_replaces_an_entry_and_registers_an_unknown_one() {
    let root = TempRoot::new();
    let registry = root.registry();

    registry.add(full_entry()).expect("add the entry");
    registry
        .update(RepositoryEntry::new(
            "acme-api",
            "https://example.com/moved.git",
        ))
        .expect("update the entry");
    registry
        .update(RepositoryEntry::new(
            "acme-web",
            "https://example.com/web.git",
        ))
        .expect("upsert a new entry");

    let stored = registry.get("acme-api").expect("read").expect("registered");
    assert_eq!(stored.remote, "https://example.com/moved.git");
    assert_eq!(registry.list().expect("list").len(), 2);
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_id_is_refused_and_leaves_the_stored_entry_untouched() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    let error = registry
        .add(RepositoryEntry::new(
            "acme-api",
            "https://example.com/other.git",
        ))
        .expect_err("a duplicate id must be refused");

    assert!(
        matches!(&error, RegistryError::DuplicateId { id, .. } if id == "acme-api"),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("acme-api"), "message: {message}");
    assert!(message.contains("repositories.json"), "message: {message}");

    // The registry still holds the original entry, byte for byte.
    assert_eq!(
        registry.get("acme-api").expect("read").expect("registered"),
        full_entry()
    );
}

#[test]
fn removing_an_unknown_id_is_reported_rather_than_silently_ignored() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    let error = registry
        .remove("no-such-repository")
        .expect_err("removing nothing must be an error");

    assert!(
        matches!(&error, RegistryError::NotFound { id, .. } if id == "no-such-repository"),
        "unexpected error: {error}"
    );
    assert!(
        error.to_string().contains("repositories.json"),
        "the message must name the file: {error}"
    );
    assert_eq!(registry.list().expect("list").len(), 1);
}

#[test]
fn remove_drops_the_entry_and_hands_it_back() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    let removed = registry.remove("acme-api").expect("remove the entry");

    assert_eq!(removed, full_entry());
    assert!(registry.list().expect("list").is_empty());
}

#[test]
fn a_remote_using_the_ext_transport_is_refused() {
    let root = TempRoot::new();
    let registry = root.registry();

    let error = registry
        .add(RepositoryEntry::new(
            "hostile",
            "ext::sh -c 'touch /tmp/pwned'",
        ))
        .expect_err("the ext:: transport makes git run an arbitrary command");

    assert!(
        matches!(&error, RegistryError::InvalidRemote { id, .. } if id == "hostile"),
        "unexpected error: {error}"
    );
    assert!(!registry.path().exists(), "nothing may have been written");
}

#[test]
fn a_remote_starting_with_a_dash_is_refused() {
    let root = TempRoot::new();
    let registry = root.registry();

    let error = registry
        .add(RepositoryEntry::new("sneaky", "--upload-pack=touch"))
        .expect_err("git reads a leading '-' as an option");

    assert!(
        matches!(&error, RegistryError::InvalidRemote { id, .. } if id == "sneaky"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_id_that_is_not_a_usable_directory_name_is_refused() {
    let root = TempRoot::new();
    let registry = root.registry();

    for id in ["", ".", "..", "nested/id", "back\\slash", " padded "] {
        let error = registry
            .add(RepositoryEntry::new(id, "https://example.com/repo.git"))
            .expect_err("an id that cannot name a directory must be refused");
        assert!(
            matches!(&error, RegistryError::InvalidId { .. }),
            "id '{id}' produced the wrong error: {error}"
        );
    }
    assert!(!registry.path().exists(), "nothing may have been written");
}

// ---------------------------------------------------------------------------
// On disk
// ---------------------------------------------------------------------------

#[test]
fn the_file_is_written_atomically_and_leaves_no_temporary_behind() {
    let root = TempRoot::new();
    let registry = root.registry();

    registry.add(full_entry()).expect("add the entry");
    registry
        .add(RepositoryEntry::new(
            "acme-web",
            "https://example.com/web.git",
        ))
        .expect("add a second entry");
    registry.remove("acme-web").expect("remove it again");

    assert!(registry.path().is_file(), "the registry file exists");
    let leftovers: Vec<PathBuf> = fs::read_dir(root.path())
        .expect("read the root")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "an atomic write must leave no .tmp behind, found: {leftovers:?}"
    );
}

#[test]
fn the_registry_lives_in_repositories_json_under_the_root() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    assert_eq!(registry.path(), root.path().join("repositories.json"));
    let raw = fs::read_to_string(registry.path()).expect("read the file");
    assert!(raw.contains("\"acme-api\""), "file was:\n{raw}");
}

#[test]
fn an_entry_converts_to_the_domain_types_without_a_third_shape() {
    let entry = full_entry();

    let reference = entry.to_reference();
    assert_eq!(reference.id, entry.id);
    assert_eq!(reference.name, entry.name);
    assert_eq!(reference.provider, entry.provider);
    assert_eq!(reference.remote, entry.remote);
    assert_eq!(reference.default_branch, entry.default_branch);

    let info = entry.to_info();
    assert_eq!(info.id, entry.id);
    assert_eq!(info.name, entry.name);
    assert_eq!(info.provider, entry.provider);
    assert_eq!(info.remote, entry.remote);
}

/// `narsor repo add` used to guess the provider from the remote while the
/// TUI form left it at the default, so the same repository registered two ways
/// described itself two ways. The guess lives beside the entry now, so both
/// paths label a remote identically.
#[test]
fn the_provider_is_inferred_from_the_remote_shape() {
    use narsor::registry::infer_provider;

    assert_eq!(infer_provider("/home/artur/Projekty/narsor"), "local");
    assert_eq!(infer_provider("./relative/repo"), "local");
    assert_eq!(infer_provider("file:///srv/repo.git"), "local");
    assert_eq!(infer_provider("git@github.com:acme/web.git"), "github");
    assert_eq!(infer_provider("https://gitlab.com/acme/web.git"), "gitlab");
    assert_eq!(infer_provider("git://example.invalid/web.git"), "git");

    // The entry constructor applies it, which is what makes the two paths agree.
    let entry = RepositoryEntry::new("demo", "/srv/demo");
    assert_eq!(entry.provider, "local");
}
