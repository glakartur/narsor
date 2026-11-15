//! The script contract (R12): what an observer's script reads on stdin and
//! what it must print on stdout.
//!
//! ```text
//! stdin   one JSON document, ScriptInput
//! stdout  one JSON document, ScriptOutput: {"state": …, "closed_watches": […]}
//! ```
//!
//! Exit code 0 with a document [`parse_script_output`] accepts is the only
//! result that changes the observer's remembered state; anything else leaves
//! it as it was (R14). The output is untrusted: it is bounded before it is
//! parsed, and anything beyond the one document — trailing text, unknown
//! fields — is refused rather than ignored.

use super::model::{ObserverId, ObserverName, MAX_WATCH_OBJECT_MAX_LEN};
use crate::domain::TaskId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version of the contract a [`ScriptInput`] follows.
pub const SCRIPT_CONTRACT_VERSION: u32 = 1;

/// Largest stdout document a script may print (1 MiB).
pub const MAX_SCRIPT_OUTPUT_BYTES: usize = 1024 * 1024;

/// Most objects one output may report closed.
pub const MAX_CLOSED_WATCHES: usize = 10_000;

/// The observer, as its script sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptObserver {
    /// Its id.
    pub id: ObserverId,
    /// Its name.
    pub name: ObserverName,
    /// The source it watches.
    pub source: String,
}

/// One watch entry, as its script sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptWatch {
    /// The watched object (a PR/MR URL, say).
    pub object: String,
    /// The task its events go to.
    pub task_id: TaskId,
    /// The event types to catch.
    #[serde(default)]
    pub event_types: Vec<String>,
    /// The id of the `wait` episode the task is in, `None` when the task is
    /// not waiting: the script sends an event only to a waiting task.
    #[serde(default)]
    pub wait_episode: Option<String>,
}

/// What the script reads on stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptInput {
    /// [`SCRIPT_CONTRACT_VERSION`].
    pub contract_version: u32,
    /// The observer.
    pub observer: ScriptObserver,
    /// The owner's condition, as configured.
    pub condition: serde_json::Value,
    /// The state the last successful run returned; `None` before the first.
    #[serde(default)]
    pub state: Option<serde_json::Value>,
    /// The watch entries, for a `watch_list` observer.
    #[serde(default)]
    pub watches: Vec<ScriptWatch>,
    /// No run of this observer has succeeded yet.
    #[serde(default)]
    pub first_run: bool,
    /// On the first run, import what already matches instead of only
    /// remembering where to start from.
    #[serde(default)]
    pub import_backlog: bool,
}

impl ScriptInput {
    /// The document as the script reads it.
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a script input always serializes")
    }
}

/// What the script prints on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptOutput {
    /// The state the next run receives. Required: a script that has nothing
    /// to remember prints `{}`.
    pub state: serde_json::Value,
    /// Watched objects that ended (a merged or closed PR/MR): their entries
    /// are removed (R7).
    #[serde(default)]
    pub closed_watches: Vec<String>,
}

/// Why a script's output broke the contract.
#[derive(Debug, Error)]
pub enum ContractError {
    /// More bytes than [`MAX_SCRIPT_OUTPUT_BYTES`].
    #[error("the script output has {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// Bytes received.
        size: usize,
        /// The limit.
        limit: usize,
    },
    /// Not UTF-8 text.
    #[error("the script output is not UTF-8 text")]
    NotUtf8,
    /// Not exactly one JSON document of the expected shape.
    #[error("the script output is not a valid result document: {0}")]
    Malformed(#[source] serde_json::Error),
    /// A `closed_watches` entry that cannot be an object.
    #[error("the script output reports an invalid closed watch: {0}")]
    InvalidClosedWatch(String),
}

/// Parses a script's stdout, refusing anything but one bounded document.
pub fn parse_script_output(bytes: &[u8]) -> Result<ScriptOutput, ContractError> {
    if bytes.len() > MAX_SCRIPT_OUTPUT_BYTES {
        return Err(ContractError::TooLarge {
            size: bytes.len(),
            limit: MAX_SCRIPT_OUTPUT_BYTES,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ContractError::NotUtf8)?;
    // `from_str` refuses trailing non-whitespace, so a second document or a
    // log line after the first is an error, not silently dropped.
    let output: ScriptOutput = serde_json::from_str(text).map_err(ContractError::Malformed)?;
    if output.closed_watches.len() > MAX_CLOSED_WATCHES {
        return Err(ContractError::InvalidClosedWatch(format!(
            "{} entries, more than the limit of {MAX_CLOSED_WATCHES}",
            output.closed_watches.len()
        )));
    }
    for object in &output.closed_watches {
        if object.is_empty()
            || object.len() > MAX_WATCH_OBJECT_MAX_LEN
            || object.chars().any(char::is_control)
        {
            return Err(ContractError::InvalidClosedWatch(format!(
                "an entry must have 1-{MAX_WATCH_OBJECT_MAX_LEN} bytes and no control characters"
            )));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_valid_document_parses_with_and_without_closed_watches() {
        let output = parse_script_output(br#"{"state": {"cursor": 5}}"#).unwrap();
        assert_eq!(output.state, json!({"cursor": 5}));
        assert!(output.closed_watches.is_empty());
        let output =
            parse_script_output(b" {\"state\": {}, \"closed_watches\": [\"MR!17\"]}\n").unwrap();
        assert_eq!(output.closed_watches, vec!["MR!17".to_string()]);
    }

    #[test]
    fn anything_but_one_document_is_refused() {
        for bytes in [
            &b"not json"[..],
            b"",
            br#"{"state": {}} {"state": {}}"#,
            br#"{"state": {}} trailing log line"#,
            br#"{"closed_watches": []}"#,
            br#"{"state": {}, "extra": 1}"#,
            br#"[1, 2]"#,
        ] {
            assert!(
                matches!(parse_script_output(bytes), Err(ContractError::Malformed(_))),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn an_oversized_or_non_utf8_output_is_refused_before_parsing() {
        let mut big = br#"{"state": ""#.to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_SCRIPT_OUTPUT_BYTES));
        big.extend(br#""}"#);
        assert!(matches!(
            parse_script_output(&big),
            Err(ContractError::TooLarge { .. })
        ));
        assert!(matches!(
            parse_script_output(b"{\"state\": \"\xff\"}"),
            Err(ContractError::NotUtf8)
        ));
    }

    #[test]
    fn a_closed_watch_must_be_a_plain_object() {
        for entry in ["\"\"", "\"a\\nb\""] {
            let document = format!(r#"{{"state": {{}}, "closed_watches": [{entry}]}}"#);
            assert!(matches!(
                parse_script_output(document.as_bytes()),
                Err(ContractError::InvalidClosedWatch(_))
            ));
        }
    }
}
