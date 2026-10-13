//! Integration tests for the repository registry.
//!
//! Everything runs against a real temporary root, because the value of this
//! layer is what actually lands on disk: one JSON document, written through the
//! atomic writer, that a missing file reads back as empty.

use narsor_engine::registry::{RegistryError, RepositoryEntry, RepositoryRegistry};
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

    assert!(!registry.dir().exists(), "nothing was written yet");
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
        .entries
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
    assert_eq!(registry.list().expect("list").entries.len(), 2);
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
    assert!(
        message.contains("acme-api.yaml"),
        "the message must name the file holding the existing entry: {message}"
    );

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
        error.to_string().contains("repositories"),
        "the message must name the directory that was searched: {error}"
    );
    assert_eq!(registry.list().expect("list").entries.len(), 1);
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
    assert!(!registry.dir().exists(), "nothing may have been written");
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
    assert!(!registry.dir().exists(), "nothing may have been written");
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

    assert!(registry.dir().is_dir(), "the registry directory exists");
    let leftovers: Vec<PathBuf> = fs::read_dir(registry.dir())
        .expect("read the registry directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "an atomic write must leave no .tmp behind, found: {leftovers:?}"
    );
}

/// R7 — one file per repository, named for the id, under the catalogue.
#[test]
fn each_repository_is_one_yaml_file_named_for_its_id() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    let path = root.path().join("repositories").join("acme-api.yaml");
    assert_eq!(registry.entry_path("acme-api"), path);
    assert!(path.is_file(), "the entry is its own file");

    let raw = fs::read_to_string(&path).expect("read the entry");
    assert!(
        raw.contains("remote: https://github.com/acme/api.git"),
        "{raw}"
    );
    assert!(raw.contains("name: ACME API"), "{raw}");
}

/// D-O — the file name is the only identifier, so `id` is not written into it.
///
/// Two places to hold one fact is two places for it to disagree: a copied file
/// would claim to be the repository it was copied from, and every read would
/// have to pick a winner.
#[test]
fn the_id_is_carried_by_the_file_name_and_not_written_into_the_file() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    let raw = fs::read_to_string(registry.entry_path("acme-api")).expect("read the entry");
    assert!(
        !raw.contains("id:"),
        "the id belongs to the file name alone, file was:\n{raw}"
    );

    // Copying the file registers a *different* repository, named for the copy.
    fs::copy(
        registry.entry_path("acme-api"),
        registry.entry_path("acme-api-fork"),
    )
    .expect("copy the entry");
    let fork = registry
        .get("acme-api-fork")
        .expect("read the copy")
        .expect("the copy is registered");
    assert_eq!(fork.id, "acme-api-fork");
    assert_eq!(fork.remote, full_entry().remote);
}

/// D-E — the entry is meant to be written by a person and by a deployment.
#[test]
fn a_hand_written_entry_with_only_the_required_fields_reads_back() {
    let root = TempRoot::new();
    let registry = root.registry();

    fs::create_dir_all(registry.dir()).expect("create the registry directory");
    fs::write(
        registry.entry_path("hand-written"),
        "name: Hand Written\nprovider: github\nremote: https://github.com/acme/hand.git\n",
    )
    .expect("write an entry by hand");

    let entry = registry
        .get("hand-written")
        .expect("read the registry")
        .expect("the entry is registered");
    assert_eq!(entry.id, "hand-written");
    assert_eq!(entry.name, "Hand Written");
    assert_eq!(entry.default_branch, None);
    assert_eq!(entry.description, None);
}

/// An absent optional field is omitted rather than written as `null`.
#[test]
fn an_entry_without_optional_fields_omits_them_instead_of_nulling_them() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry
        .add(RepositoryEntry::new(
            "minimal",
            "https://example.com/minimal.git",
        ))
        .expect("add a minimal entry");

    let raw = fs::read_to_string(registry.entry_path("minimal")).expect("read the entry");
    assert!(!raw.contains("default_branch"), "{raw}");
    assert!(!raw.contains("description"), "{raw}");
    assert!(!raw.contains("null"), "{raw}");
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
    use narsor_engine::registry::infer_provider;

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

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Number of writers racing for one catalogue in the concurrency tests.
const WRITERS: usize = 8;

/// How many times a race is replayed before it is believed.
///
/// A lost update needs the second writer to read before the first one renames,
/// which is a window of microseconds. One attempt says nothing; a few dozen
/// starting from a shared barrier hit it reliably. The cost is a few hundred
/// files in a temporary directory, which is nothing beside a defect that
/// presents as "an entry I registered is not there".
const ATTEMPTS: usize = 40;

/// AE5 — two processes registering *different* repositories keep both entries.
///
/// This is the defect the registry rewrite exists to fix, so the test is
/// written against the behaviour rather than the storage: it says nothing
/// about one document or many files, only that registering a repository
/// nobody else is registering succeeds and stays registered.
///
/// A read-modify-write registry breaks it in two different ways, and the
/// assertions are split so a failure says which one happened. Either the last
/// writer renames a document it assembled before the others had written theirs
/// — a silent lost update — or two writers collide on the *temporary* file,
/// whose name is derived from the target, and one of them renames a file the
/// other already moved away. The second is not silent: it surfaces as a
/// spurious failure to register.
#[test]
fn concurrent_registration_of_different_repositories_keeps_every_entry() {
    for attempt in 0..ATTEMPTS {
        let root = TempRoot::new();
        let registry = root.registry();
        let barrier = std::sync::Barrier::new(WRITERS);

        // Collected rather than unwrapped inside the thread: a panic in a
        // scoped thread aborts the scope and hides whether the *other* writers
        // fared any better, which is the more interesting half of the answer.
        let outcomes: Vec<std::result::Result<(), String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let barrier = &barrier;
                    let registry = &registry;
                    scope.spawn(move || {
                        // Every writer is released at the same instant;
                        // staggered starts would serialize and prove nothing.
                        barrier.wait();
                        registry
                            .add(RepositoryEntry::new(
                                format!("repo-{writer}"),
                                format!("https://example.com/{writer}.git"),
                            ))
                            .map_err(|error| format!("repo-{writer}: {error}"))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a writer thread must not panic"))
                .collect()
        });

        let refused: Vec<&String> = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().err())
            .collect();
        assert!(
            refused.is_empty(),
            "attempt {attempt}: every writer registered an id nobody else was \
             registering, so none of them may be refused; refused: {refused:?}"
        );

        let stored: Vec<String> = registry
            .list()
            .expect("list the registry")
            .entries
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        let expected: Vec<String> = (0..WRITERS)
            .map(|writer| format!("repo-{writer}"))
            .collect();

        assert_eq!(
            stored, expected,
            "attempt {attempt}: every add reported success, so every entry must \
             be there; a missing one is a lost update"
        );
    }
}

/// The same eight registrations, one thread, as a control.
///
/// Without this a failure above is ambiguous between "the registry loses
/// concurrent writes" and "the registry cannot hold eight entries at all".
#[test]
fn eight_sequential_registrations_keep_every_entry() {
    let root = TempRoot::new();
    let registry = root.registry();

    for writer in 0..WRITERS {
        registry
            .add(RepositoryEntry::new(
                format!("repo-{writer}"),
                format!("https://example.com/{writer}.git"),
            ))
            .expect("register a repository");
    }

    assert_eq!(
        registry.list().expect("list the registry").entries.len(),
        WRITERS
    );
}

/// AE6 — two writers racing for the *same* id: one wins, the other is told.
///
/// The sequential version of this is asserted above; this is the one that
/// catches a collision on the temporary file. Deriving the temporary name from
/// the target alone would let both writers open the same scratch file, and the
/// entry the winner links into place could then hold a splice of two
/// documents — a duplicate correctly refused, on top of a first entry that is
/// no longer what was registered.
#[test]
fn concurrent_registration_of_one_id_leaves_exactly_the_winner_on_disk() {
    for attempt in 0..ATTEMPTS {
        let root = TempRoot::new();
        let registry = root.registry();
        let barrier = std::sync::Barrier::new(WRITERS);

        let outcomes: Vec<std::result::Result<usize, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let barrier = &barrier;
                    let registry = &registry;
                    scope.spawn(move || {
                        barrier.wait();
                        registry
                            .add(
                                RepositoryEntry::new("contested", "https://example.com/one.git")
                                    .with_description(format!("written by {writer}")),
                            )
                            .map(|()| writer)
                            .map_err(|error| error.to_string())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a writer thread must not panic"))
                .collect()
        });

        let winners: Vec<usize> = outcomes
            .iter()
            .filter_map(|o| o.as_ref().ok())
            .copied()
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "attempt {attempt}: exactly one writer may create the entry, got {winners:?}"
        );
        for refusal in outcomes.iter().filter_map(|o| o.as_ref().err()) {
            assert!(
                refusal.contains("already registered"),
                "attempt {attempt}: a loser must be told the id is taken, got: {refusal}"
            );
        }

        // The survivor is exactly one writer's entry, not a splice of several.
        let stored = registry
            .get("contested")
            .expect("read the contested entry")
            .expect("the entry is registered");
        assert_eq!(
            stored.description,
            Some(format!("written by {}", winners[0])),
            "attempt {attempt}: the stored entry must be the winner's, whole"
        );
    }
}

// ---------------------------------------------------------------------------
// One bad entry
// ---------------------------------------------------------------------------

/// Writes a file straight into the registry directory, bypassing the registry.
fn plant(registry: &RepositoryRegistry, name: &str, contents: &str) -> PathBuf {
    fs::create_dir_all(registry.dir()).expect("create the registry directory");
    let path = registry.dir().join(name);
    fs::write(&path, contents).expect("plant a file");
    path
}

/// AE7 — a file that will not parse costs one repository, not the catalogue.
#[test]
fn a_corrupt_entry_is_named_by_the_listing_and_the_rest_still_load() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add a good entry");
    registry
        .add(RepositoryEntry::new(
            "acme-web",
            "https://example.com/web.git",
        ))
        .expect("add a second good entry");
    let broken = plant(&registry, "broken.yaml", "this: is: not: valid: yaml:\n");

    let listing = registry.list().expect("list the registry");

    let ids: Vec<&str> = listing.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["acme-api", "acme-web"], "the good entries still load");
    assert_eq!(
        listing.failures.len(),
        1,
        "failures: {:?}",
        listing.failures
    );
    assert_eq!(listing.failures[0].path, broken);
    assert!(!listing.is_ok(), "a listing with a failure is not ok");
}

/// AE7 — a file that parses but is not an entry fails the same way.
///
/// "Broken YAML" and "YAML that is missing `remote`" need no distinguishing:
/// both are a repository that cannot be cloned, and both end the listing the
/// same way.
#[test]
fn an_incomplete_entry_fails_like_an_unparsable_one() {
    let root = TempRoot::new();
    let registry = root.registry();
    let incomplete = plant(&registry, "half.yaml", "name: Half\nprovider: github\n");

    let listing = registry.list().expect("list the registry");

    assert!(listing.entries.is_empty());
    assert_eq!(listing.failures.len(), 1);
    assert_eq!(listing.failures[0].path, incomplete);
    assert!(
        listing.failures[0].reason.contains("remote"),
        "the reason should name the missing field: {:?}",
        listing.failures[0]
    );
}

/// D-H — a listing may degrade; a lookup by id may not.
///
/// A workflow names a repository by id. Answering "not registered" for a
/// repository that *is* registered sends it to the raw-remote fallback, which
/// clones something else entirely — an approximate answer is worse than none.
#[test]
fn reading_one_corrupt_entry_by_id_fails_rather_than_reporting_it_absent() {
    let root = TempRoot::new();
    let registry = root.registry();
    plant(&registry, "acme-api.yaml", "this: is: not: valid: yaml:\n");

    let error = registry
        .get("acme-api")
        .expect_err("a corrupt entry must not read as absent");

    assert!(
        matches!(&error, RegistryError::Corrupt { .. }),
        "unexpected error: {error}"
    );
    assert!(error.to_string().contains("acme-api"), "{error}");
}

/// R10 — the id is checked on the way out too, not only on the way in.
///
/// A directory is a shared surface; an entry can arrive in it without ever
/// passing through this crate.
#[test]
fn a_file_whose_name_is_not_a_usable_id_is_a_failure_not_an_entry() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add a good entry");
    let hostile = plant(
        &registry,
        " padded .yaml",
        "name: X\nprovider: git\nremote: /srv/x\n",
    );

    let listing = registry.list().expect("list the registry");

    assert_eq!(listing.entries.len(), 1, "only the good entry loads");
    assert_eq!(listing.failures.len(), 1);
    assert_eq!(listing.failures[0].path, hostile);
}

/// D-L — anything that is not a definition file is not the registry's business.
#[test]
fn files_that_are_not_entries_are_ignored_without_complaint() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add a good entry");
    plant(&registry, "README", "how this directory works\n");
    plant(&registry, "notes.txt", "scratch\n");
    plant(&registry, "acme-api.yaml.8f2c.tmp", "half a wri");

    let listing = registry.list().expect("list the registry");

    assert_eq!(listing.entries.len(), 1);
    assert!(
        listing.is_ok(),
        "a README is not a broken repository: {:?}",
        listing.failures
    );
}

/// R9 — a mutation reads and writes one entry and no other.
#[test]
fn updating_one_entry_does_not_touch_any_other_file() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");
    registry
        .add(RepositoryEntry::new(
            "acme-web",
            "https://example.com/web.git",
        ))
        .expect("add a second entry");

    let untouched = registry.entry_path("acme-web");
    let before = fs::read(&untouched).expect("read the neighbour");

    registry
        .update(RepositoryEntry::new(
            "acme-api",
            "https://example.com/moved.git",
        ))
        .expect("update the first entry");

    assert_eq!(
        fs::read(&untouched).expect("read the neighbour again"),
        before,
        "updating one entry must leave its neighbours byte for byte alone"
    );
}

/// R12 — the old single document is not read, converted, or mentioned.
#[test]
fn an_old_repositories_json_is_ignored_entirely() {
    let root = TempRoot::new();
    let registry = root.registry();
    fs::write(
        root.path().join("repositories.json"),
        r#"{"repositories":[{"id":"legacy","name":"Legacy","provider":"git","remote":"/srv/legacy"}]}"#,
    )
    .expect("plant an old registry");

    let listing = registry.list().expect("list the registry");

    assert!(listing.is_empty(), "the old document is not a registry");
    assert_eq!(registry.get("legacy").expect("look up the old id"), None);
}

/// A lookup key that cannot name a file is absent, not invalid.
///
/// `globals.workspace.repository` is looked up here *first* and falls back to
/// being treated as a raw git remote. Most values reaching this function are
/// therefore URLs and paths that were never meant to be ids; refusing them
/// would break the fallback and stop a workflow that names a remote directly.
/// `add` still refuses the same strings — an operator naming a repository is
/// making a different claim than a workflow resolving one.
#[test]
fn looking_up_something_that_cannot_be_an_id_reports_absence_not_a_refusal() {
    let root = TempRoot::new();
    let registry = root.registry();
    registry.add(full_entry()).expect("add the entry");

    for lookup in [
        "/srv/checkouts/source-repo",
        "https://github.com/acme/api.git",
        "git@github.com:acme/api.git",
        "../escape",
        "",
    ] {
        assert_eq!(
            registry.get(lookup).expect("a raw remote is not an error"),
            None,
            "'{lookup}' is not registered, which is not the same as invalid"
        );
    }

    // The same strings are still refused when registering one.
    let error = registry
        .add(RepositoryEntry::new(
            "../escape",
            "https://example.com/x.git",
        ))
        .expect_err("an operator naming an unusable id must be told");
    assert!(matches!(&error, RegistryError::InvalidId { .. }), "{error}");
}

// ---------------------------------------------------------------------------
// Base image and Git credential (U6, R15, R39)
// ---------------------------------------------------------------------------

const PINNED: &str =
    "ghcr.io/acme/api-dev@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn a_base_image_and_git_credential_round_trip_in_both_policies() {
    let root = TempRoot::new();
    let registry = root.registry();
    let pinned = full_entry()
        .with_base_image(narsor_engine::domain::ImageSpec::pinned(PINNED))
        .with_git_credential("github-token");
    registry.add(pinned.clone()).expect("add");
    assert_eq!(registry.get("acme-api").unwrap(), Some(pinned));

    let text = fs::read_to_string(registry.entry_path("acme-api")).unwrap();
    assert!(
        text.contains(&format!("base_image: {PINNED}")),
        "a pinned image is written as a plain reference: {text}"
    );

    let resolving = full_entry().with_base_image(narsor_engine::domain::ImageSpec::resolve(
        "ghcr.io/acme/api-dev:2026.10",
    ));
    registry.update(resolving.clone()).expect("update");
    assert_eq!(registry.get("acme-api").unwrap(), Some(resolving));
}

#[test]
fn an_unusable_base_image_is_refused_when_registered() {
    let root = TempRoot::new();
    let registry = root.registry();
    for image in [
        narsor_engine::domain::ImageSpec::pinned("ghcr.io/acme/api-dev:2026.10"),
        narsor_engine::domain::ImageSpec::resolve("ghcr.io/acme/api-dev"),
        narsor_engine::domain::ImageSpec::pinned("--privileged@sha256:00"),
    ] {
        let error = registry
            .add(full_entry().with_base_image(image.clone()))
            .expect_err("refused");
        assert!(
            matches!(error, RegistryError::InvalidBaseImage { .. }),
            "{image:?}: {error}"
        );
    }
    assert!(registry.list().unwrap().is_empty(), "nothing was written");
}

#[test]
fn an_entry_written_before_base_images_still_reads() {
    let root = TempRoot::new();
    let registry = root.registry();
    fs::create_dir_all(registry.dir()).unwrap();
    fs::write(
        registry.entry_path("legacy"),
        "name: legacy\nprovider: git\nremote: https://example.com/legacy.git\n",
    )
    .unwrap();
    let entry = registry.get("legacy").unwrap().expect("present");
    assert_eq!(entry.base_image, None);
    assert_eq!(entry.git_credential, None);
}

#[test]
fn a_described_entry_defaults_the_name_and_the_provider_and_is_validated_on_add() {
    let entry = RepositoryEntry::described(
        "acme",
        "https://github.com/acme/api.git",
        None,
        None,
        Some("main".to_string()),
        None,
    );
    assert_eq!(entry.name, "acme");
    assert_eq!(entry.provider, "github");
    assert_eq!(entry.default_branch.as_deref(), Some("main"));

    let root = TempRoot::new();
    let registry = RepositoryRegistry::new(root.path());
    let hostile = RepositoryEntry::described("evil", "--upload-pack=x", None, None, None, None);
    assert!(matches!(
        registry.add(hostile),
        Err(RegistryError::InvalidRemote { .. })
    ));
    registry.add(entry).expect("registered");
}
