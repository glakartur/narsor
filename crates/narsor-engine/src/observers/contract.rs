//! The script contract (R12): what an observer's script reads on stdin and
//! what its result is (D18).
//!
//! ```text
//! stdin   one JSON document, ScriptInput
//!
//! an observer in mode `always`:
//!   stdout  one JSON document, ScriptOutput: {"state": …}
//!   exit 0 + a document parse_script_output accepts → the state is saved
//!   anything else                                   → error, state kept
//!
//! an observer in mode `on_episodes`: one run checks one queue entry
//!   stdout  not read
//!   exit 0 (ENTRY_DONE_EXIT_CODE)    → the run is done
//!   exit 1 (ENTRY_PENDING_EXIT_CODE) → the run is done (as for exit 0)
//!   any other result                 → error, recorded for the entry
//! ```
//!
//! No exit code removes an entry: the entry goes when its gate exits
//! (R14, R16), and is checked again in the next round until then.
//!
//! For an observer that creates tasks, exit code 0 with a valid document is
//! the only result that changes the remembered state (R14). The output is
//! untrusted: it is bounded before it is parsed, and anything beyond the one
//! document — trailing text, unknown fields — is refused rather than
//! ignored.

use super::model::{ObserverId, ObserverName};
use crate::domain::TaskId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// Version of the contract a [`ScriptInput`] follows.
pub const SCRIPT_CONTRACT_VERSION: u32 = 1;

/// Largest stdout document a script may print (1 MiB).
pub const MAX_SCRIPT_OUTPUT_BYTES: usize = 1024 * 1024;

/// The exit code of a run that checked its queue entry.
pub const ENTRY_DONE_EXIT_CODE: i32 = 0;

/// The exit code a script for a watch list gave when its object had to be
/// checked again; a run for a queue entry may still give it.
pub const ENTRY_PENDING_EXIT_CODE: i32 = 1;

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

/// One queue entry, as its script sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptEntry {
    /// The task waiting at the gate.
    pub task_id: TaskId,
    /// The wait episode: an answer names it.
    pub episode: String,
    /// The workflow state of the gate.
    pub step: String,
    /// The gate's rendered params.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
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
    /// The one queue entry this run checks, for an `on_episodes` observer;
    /// `None` for an `always` observer.
    #[serde(default)]
    pub entry: Option<ScriptEntry>,
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

/// What the script of an observer that creates tasks prints on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptOutput {
    /// The state the next run receives. Required: a script that has nothing
    /// to remember prints `{}`.
    pub state: serde_json::Value,
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
    serde_json::from_str(text).map_err(ContractError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_valid_document_parses() {
        let output = parse_script_output(b" {\"state\": {\"cursor\": 5}}\n").unwrap();
        assert_eq!(output.state, json!({"cursor": 5}));
    }

    #[test]
    fn anything_but_one_document_is_refused() {
        for bytes in [
            &b"not json"[..],
            b"",
            br#"{"state": {}} {"state": {}}"#,
            br#"{"state": {}} trailing log line"#,
            br#"{"closed_watches": []}"#,
            br#"{"state": {}, "closed_watches": []}"#,
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
}
