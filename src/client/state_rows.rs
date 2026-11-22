//! Flattens the state of an execution into display rows.
//!
//! Each row is one path with one text value. The web UI has a twin of this
//! logic; both read the cases of `state-rows.json`.

use narsor_protocol::controller::{ExecutionState, StateSection};
use serde_json::Value;

/// One line item of the state view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    /// Keys and array indexes joined with `:`, starting with the section name.
    pub path: String,
    /// The value as text: a string is raw, everything else is compact JSON.
    pub value: String,
    /// Number of display lines of `value`.
    pub lines: usize,
}

/// Rows of the whole state: `context`, then `locals`, then `conductor`.
/// Inside a section, rows follow the path order (keys by code unit, array
/// indexes by number).
pub fn state_rows(state: &ExecutionState) -> Vec<StateRow> {
    let mut rows = Vec::new();
    for (name, section) in [
        ("context", &state.context),
        ("locals", &state.locals),
        ("conductor", &state.conductor),
    ] {
        match section {
            StateSection::Available { value } => flatten(name, value, &mut rows),
            StateSection::Absent { message } => push(name, format!("({message})"), &mut rows),
            StateSection::Unreadable { reason } => push(name, format!("({reason})"), &mut rows),
        }
    }
    rows
}

fn push(path: &str, value: String, rows: &mut Vec<StateRow>) {
    let lines = value.split('\n').count();
    rows.push(StateRow {
        path: path.to_string(),
        value,
        lines,
    });
}

fn flatten(path: &str, value: &Value, rows: &mut Vec<StateRow>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                flatten(&format!("{path}:{key}"), &map[key], rows);
            }
        }
        Value::Array(items) if items.iter().any(|item| item.is_object()) => {
            for (index, item) in items.iter().enumerate() {
                flatten(&format!("{path}:{index}"), item, rows);
            }
        }
        Value::String(text) => push(path, text.clone(), rows),
        other => push(path, other.to_string(), rows),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_every_fixture_case_into_the_expected_rows() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../crates/narsor-web/ui-tests/fixtures/state-rows.json"
        ))
        .expect("fixture is valid JSON");
        let cases = fixture["cases"].as_array().expect("cases array");
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let state: ExecutionState = serde_json::from_value(case["state"].clone())
                .unwrap_or_else(|e| panic!("{name}: state does not parse: {e}"));
            let expected: Vec<(String, String, usize)> = case["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r["path"].as_str().unwrap().to_string(),
                        r["value"].as_str().unwrap().to_string(),
                        r["lines"].as_u64().unwrap() as usize,
                    )
                })
                .collect();
            let actual: Vec<(String, String, usize)> = state_rows(&state)
                .into_iter()
                .map(|r| (r.path, r.value, r.lines))
                .collect();
            assert_eq!(actual, expected, "{name}");
        }
    }
}
