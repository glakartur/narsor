//! The install one-liner in the docs pins a release tag: every release is a
//! pre-release, so `/releases/latest/download/` answers 404. These checks keep
//! the pinned tag well-formed and identical wherever it is spelled out.

use std::{fs, path::PathBuf, process::Command};

const PREFIX: &str = "releases/download/";
const SUFFIX: &str = "/install.sh";
const DOCS: &[&str] = &["README.md", "docs/operations/installation.md"];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn pinned_tags(text: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        rest = &rest[at + PREFIX.len()..];
        if let Some(end) = rest.find(SUFFIX) {
            tags.push(rest[..end].to_string());
        }
    }
    tags
}

fn is_release_tag(tag: &str) -> bool {
    let Some(version) = tag.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[test]
fn the_docs_pin_one_well_formed_release_tag() {
    let mut all = Vec::new();
    for doc in DOCS {
        let text = fs::read_to_string(root().join(doc)).expect("read doc");
        let tags = pinned_tags(&text);
        assert!(!tags.is_empty(), "{doc} has no tag-pinned install URL");
        all.extend(tags);
    }
    for tag in &all {
        assert!(is_release_tag(tag), "`{tag}` is not of the form vX.Y.Z");
    }
    assert!(
        all.iter().all(|tag| tag == &all[0]),
        "the docs pin different tags: {all:?}"
    );
}

#[test]
fn the_readme_never_points_at_the_latest_download_url() {
    let text = fs::read_to_string(root().join("README.md")).expect("read README");
    assert!(
        !text.contains("releases/latest/download"),
        "README must pin a tag; /releases/latest/ ignores pre-releases"
    );
}

#[test]
fn the_pinned_tag_exists_when_tags_are_available() {
    let text = fs::read_to_string(root().join("README.md")).expect("read README");
    let tag = pinned_tags(&text).into_iter().next().expect("pinned tag");
    let Ok(output) = Command::new("git")
        .args(["tag", "--list"])
        .current_dir(root())
        .output()
    else {
        return;
    };
    let tags = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || tags.trim().is_empty() {
        return;
    }
    assert!(
        tags.lines().any(|line| line == tag),
        "tag {tag} is not among the repository tags"
    );
}
