//! Relative links and heading anchors in the maintained markdown docs must
//! resolve. Dated working artifacts (`docs/brainstorms`, `docs/plans`, …) are
//! not checked.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn markdown_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root()).expect("read root") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
    for dir in ["docs/user", "docs/dev", "docs/operations"] {
        for entry in fs::read_dir(root().join(dir)).expect("read docs dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.push(root().join("docs/README.md"));
    files
}

fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn anchors(text: &str) -> HashSet<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let title = line.trim_start_matches('#');
        if !title.starts_with(' ') {
            continue;
        }
        let base = slug(title);
        let count = seen.iter().filter(|s| **s == base).count();
        seen.push(base.clone());
        if count > 0 {
            seen.push(format!("{base}-{count}"));
        }
    }
    seen.into_iter().collect()
}

fn links(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("](") {
            rest = &rest[at + 2..];
            if let Some(end) = rest.find(')') {
                let target = rest[..end].split_whitespace().next().unwrap_or("");
                found.push(target.to_string());
                rest = &rest[end..];
            }
        }
    }
    found
}

#[test]
fn relative_links_and_anchors_resolve() {
    let mut broken = Vec::new();
    for file in markdown_files() {
        let text = fs::read_to_string(&file).unwrap_or_else(|_| panic!("read {file:?}"));
        for link in links(&text) {
            if link.is_empty() || link.contains("://") || link.starts_with("mailto:") {
                continue;
            }
            let (path_part, anchor) = match link.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (link.as_str(), None),
            };
            let target: PathBuf = if path_part.is_empty() {
                file.clone()
            } else {
                file.parent().expect("parent").join(path_part)
            };
            if !target.exists() {
                broken.push(format!("{}: missing {link}", rel(&file)));
                continue;
            }
            if let Some(anchor) = anchor {
                if target.extension().is_some_and(|ext| ext == "md") {
                    let body = fs::read_to_string(&target).expect("read target");
                    if !anchors(&body).contains(anchor) {
                        broken.push(format!("{}: no anchor {link}", rel(&file)));
                    }
                }
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

fn rel(path: &Path) -> String {
    path.strip_prefix(root())
        .unwrap_or(path)
        .display()
        .to_string()
}
