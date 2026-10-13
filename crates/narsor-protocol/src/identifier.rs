//! Validated identifiers shared by clients, control-plane records, and server.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Longest identifier accepted for a task or backend.
pub const MAX_IDENTIFIER_LEN: usize = 128;

/// A task or backend identifier that is not safe to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdentifier {
    pub kind: &'static str,
    pub value: String,
}

impl fmt::Display for InvalidIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} identifier '{}': use 1-{} characters of [A-Za-z0-9._-], not starting with '.'", self.kind, self.value, MAX_IDENTIFIER_LEN)
    }
}

impl std::error::Error for InvalidIdentifier {}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), InvalidIdentifier> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

macro_rules! identifier {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps `value`.
            pub fn parse(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
                let value = value.into();
                validate_identifier($kind, &value)?;
                Ok(Self(value))
            }

            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self { value.0 }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
    };
}

identifier!(
    /// Stable identifier the control plane gives one logical task. Values
    /// validate on construction and deserialization before use as path parts.
    TaskId,
    "task"
);

identifier!(
    /// Stable identifier of a registered execution backend.
    BackendId,
    "backend"
);

#[cfg(test)]
mod tests {
    use super::{BackendId, TaskId};

    #[test]
    fn validated_identifier_wire_values_remain_plain_strings() {
        let task = TaskId::parse("task-1").unwrap();
        assert_eq!(serde_json::to_string(&task).unwrap(), "\"task-1\"");
        assert_eq!(serde_json::from_str::<TaskId>("\"task-1\"").unwrap(), task);
        assert!(serde_json::from_str::<TaskId>("\"../secret\"").is_err());
        assert!(serde_json::from_str::<BackendId>("\".hidden\"").is_err());
    }
}
