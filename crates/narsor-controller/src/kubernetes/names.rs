//! Deterministic, DNS-1123-safe names for a task's Kubernetes objects (R7).
//!
//! A task id may hold uppercase letters, `_` and `.`, none of which a Pod
//! name may contain, so the name cannot simply *be* the key as it is for
//! the local adapter. It is a readable, sanitized prefix of the key plus a
//! SHA-256 suffix of the *unsanitized* key: sanitizing alone would map
//! `Task_1` and `task-1` to the same name, and the hash keeps two keys
//! apart however alike their sanitized forms look (injective in practice:
//! a collision needs a 64-bit hash collision).
//!
//! Every name stays a DNS-1123 *label* (63 characters), the strictest form
//! Kubernetes uses for any of them, so the same name works as a Pod name, a
//! PVC name and inside label values.

use sha2::{Digest, Sha256};

/// Prefix of every name, so an operator recognizes Narsor's objects.
const PREFIX: &str = "narsor-";

/// Characters of the readable part.
const READABLE_LEN: usize = 30;

/// Hex characters of the hash suffix (64 bits).
const HASH_LEN: usize = 16;

/// Suffix of the storage (PVC) name.
pub const STORAGE_SUFFIX: &str = "-storage";

/// Suffix of the read-only runner's Pod name (D63).
pub const READER_SUFFIX: &str = "-reader";

/// Infix of a per-incarnation launch Secret's name.
pub const LAUNCH_INFIX: &str = "-launch-";

/// Lowercase hex SHA-256 of `text`.
pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The key's hash as it appears in names and in the task label.
pub fn key_hash(resource_key: &str) -> String {
    sha256_hex(resource_key)[..HASH_LEN].to_string()
}

/// The workload (Pod) name of a resource key.
pub fn workload(resource_key: &str) -> String {
    let mut readable = String::with_capacity(READABLE_LEN);
    for ch in resource_key.chars() {
        if readable.len() == READABLE_LEN {
            break;
        }
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            readable.push(ch);
        } else if !readable.ends_with('-') && !readable.is_empty() {
            readable.push('-');
        }
    }
    let readable = readable.trim_end_matches('-');
    if readable.is_empty() {
        format!("{PREFIX}{}", key_hash(resource_key))
    } else {
        format!("{PREFIX}{readable}-{}", key_hash(resource_key))
    }
}

/// The read-only runner's Pod name of a resource key: the workload's name
/// and [`READER_SUFFIX`] — still a DNS-1123 label (at most 61 characters),
/// and recognizably the reader of that very task's storage, which is what
/// the admission policy lets such a Pod mount.
pub fn reader(resource_key: &str) -> String {
    format!("{}{READER_SUFFIX}", workload(resource_key))
}

/// The storage (PVC) name of a resource key.
pub fn storage(resource_key: &str) -> String {
    format!("{}{STORAGE_SUFFIX}", workload(resource_key))
}

/// The launch Secret of one incarnation of a workload: unique per
/// incarnation, so a relaunch never reuses (or races) its predecessor's.
pub fn launch_secret(workload: &str, incarnation: &str) -> String {
    format!("{workload}{LAUNCH_INFIX}{}", &sha256_hex(incarnation)[..12])
}

/// Whether `name` is a DNS-1123 label.
pub fn is_dns_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Whether `name` is a DNS-1123 subdomain (the form of a Secret's name).
pub fn is_dns_subdomain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !part.starts_with('-')
                && !part.ends_with('-')
        })
}

/// Whether `key` is a valid key of a Secret's data.
pub fn is_secret_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 253
        && key != "."
        && key != ".."
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels_for_every_task_id_shape() {
        for key in [
            "task-1",
            "Task_1",
            "UPPER.case_and.dots",
            "a",
            "1",
            "_",
            "-",
            &"x".repeat(128),
            "Ünïcode-is-not-a-task-id-but-is-handled",
        ] {
            let workload = workload(key);
            let storage = storage(key);
            let reader = reader(key);
            assert!(is_dns_label(&reader), "{key} → {reader}");
            assert_ne!(reader, workload);
            assert!(is_dns_subdomain(&launch_secret(&reader, "i")));
            assert!(is_dns_label(&workload), "{key} → {workload}");
            assert!(is_dns_label(&storage), "{key} → {storage}");
            let secret = launch_secret(&workload, "incarnation");
            assert!(is_dns_subdomain(&secret), "{secret}");
        }
    }

    #[test]
    fn names_are_deterministic_and_keep_alike_keys_apart() {
        assert_eq!(workload("Task_1"), workload("Task_1"));
        let alike = ["task-1", "Task_1", "task.1", "TASK-1", "task_1"];
        let names: std::collections::HashSet<_> = alike.iter().map(|k| workload(k)).collect();
        assert_eq!(names.len(), alike.len(), "{names:?}");
        assert!(workload("Task_1").starts_with("narsor-task-1-"));
    }

    #[test]
    fn dns_checks() {
        assert!(is_dns_label("narsor-a-1"));
        assert!(!is_dns_label("Narsor"));
        assert!(!is_dns_label("-a"));
        assert!(!is_dns_label(&"a".repeat(64)));
        assert!(is_dns_subdomain("narsor-github.tokens"));
        assert!(!is_dns_subdomain("a..b"));
        assert!(is_secret_key("token.json"));
        assert!(!is_secret_key("a/b"));
        assert!(!is_secret_key(".."));
    }
}
