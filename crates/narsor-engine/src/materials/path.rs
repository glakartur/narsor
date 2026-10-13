//! What a path in a material manifest may look like.
//!
//! A material path is joined below a directory the receiver chose — on the
//! runner's task storage, in the owner's download folder — so it must not
//! be able to name anything outside it, nor anything a tool would treat as
//! active configuration of a repository. The rules are deliberately
//! narrower than any filesystem: what is refused here is refused before a
//! single byte is written anywhere, and a refused upload is cheap to fix
//! while a file written to the wrong place is not.

use super::{MaterialError, MaterialResult};

/// Longest path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Longest single component, in bytes (the common filesystem limit).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Most components in one path.
pub const MAX_DEPTH: usize = 32;

/// Checks that `path` is a normalized relative path of a regular file.
///
/// Refused: the empty path; an absolute path; empty components (a leading,
/// trailing or doubled `/`); `.` and `..`; NUL and every other control
/// character; a backslash or a colon anywhere, so that no platform reads
/// the path as having another separator, a drive or a stream; components
/// longer than [`MAX_COMPONENT_BYTES`], paths longer than
/// [`MAX_PATH_BYTES`] or deeper than [`MAX_DEPTH`]; and a `.git` component
/// in any letter case, so a material can never plant hooks or configuration
/// into a repository it is downloaded into.
pub fn validate_path(path: &str) -> MaterialResult<()> {
    let refuse = |reason: &'static str| {
        Err(MaterialError::InvalidPath {
            path: path.escape_debug().to_string(),
            reason,
        })
    };
    if path.is_empty() {
        return refuse("is empty");
    }
    if path.len() > MAX_PATH_BYTES {
        return refuse("is longer than 1024 bytes");
    }
    if path.starts_with('/') {
        return refuse("is absolute");
    }
    if path.chars().any(char::is_control) {
        return refuse("contains a control character");
    }
    if path.contains('\\') {
        return refuse("contains a backslash");
    }
    if path.contains(':') {
        return refuse("contains a colon");
    }
    let components: Vec<&str> = path.split('/').collect();
    if components.len() > MAX_DEPTH {
        return refuse("is deeper than 32 components");
    }
    for component in components {
        match component {
            "" => return refuse("has an empty component"),
            "." | ".." => return refuse("has a '.' or '..' component"),
            _ if component.len() > MAX_COMPONENT_BYTES => {
                return refuse("has a component longer than 255 bytes")
            }
            _ if component.eq_ignore_ascii_case(".git") => return refuse("has a '.git' component"),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_relative_paths_pass() {
        for path in [
            "a",
            "docs/spec.md",
            "a/b/c.txt",
            "ünïcode/файл",
            ".env",
            "a..b",
        ] {
            assert!(validate_path(path).is_ok(), "{path}");
        }
    }

    #[test]
    fn escapes_and_tricks_are_refused() {
        for path in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "a/",
            "a\\..\\b",
            "C:x",
            "a\0b",
            "a\nb",
            ".git/hooks/pre-commit",
            "sub/.GIT/config",
        ] {
            assert!(validate_path(path).is_err(), "{path:?}");
        }
        assert!(validate_path("a/".repeat(40).trim_end_matches('/')).is_err());
        assert!(validate_path(&"x".repeat(256)).is_err());
    }
}
