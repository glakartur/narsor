//! Reading an answer out of a finished harness run, whatever its shape.
//!
//! The agent's text is untrusted, so every scan here is bounded: a text full
//! of stray `{` costs at most [`MAX_BRACE_CANDIDATES`] scans to its end, never
//! a quadratic number of them. What makes an object *the* answer is the
//! executor's business; these functions only find the objects.

use serde_json::Value;

use super::super::harness::HarnessRun;
use crate::domain::ActionError;

/// Reads the answer of a finished run: its `structured_output` first, when
/// the harness reported one that `structured` accepts, then its text.
///
/// A `structured_output` that `structured` rejects is not an error of its
/// own: the text is read instead, and only its failure is reported.
pub(crate) fn structured_then_text<T>(
    run: &HarnessRun,
    structured: impl FnOnce(&Value) -> Result<T, ActionError>,
    text: impl FnOnce(&str) -> Result<T, ActionError>,
) -> Result<T, ActionError> {
    if let Some(value) = &run.structured_output {
        match structured(value) {
            Ok(answer) => return Ok(answer),
            Err(error) => tracing::warn!(
                reason = %error.message,
                "the harness's structured output is not a usable answer; reading the text instead"
            ),
        }
    }
    text(&run.text)
}

/// How many `{` that never close a scan of an agent's text skips, and how
/// many candidates it tries from the right, before it gives up.
///
/// Each one costs a scan to the end of the text, so without a bound a text
/// full of stray braces takes quadratic time. Prose has a few; the contract
/// is near the end.
const MAX_BRACE_CANDIDATES: usize = 64;

/// Every top-level JSON object in `text`, in the order they appear.
///
/// Scanning for balanced braces — string- and escape-aware, so a `}` inside a
/// string never ends an object — rather than looking for fences: it finds the
/// fenced case too, since the fence markers are outside the braces. A `{`
/// that never closes (a brace in the prose) is skipped, and the scan goes on
/// from the next one, so it cannot hide an object after it — up to
/// [`MAX_BRACE_CANDIDATES`] of them.
pub(crate) fn json_objects(text: &str) -> Vec<Value> {
    let mut found = Vec::new();
    for (start, end) in top_level_objects(text) {
        if let Ok(value) = serde_json::from_str::<Value>(&text[start..end]) {
            if value.is_object() {
                found.push(value);
            }
        }
    }
    found
}

/// The spans of the top-level balanced objects of `text`, skipping at most
/// [`MAX_BRACE_CANDIDATES`] braces that never close.
pub(crate) fn top_level_objects(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let (mut from, mut unclosed) = (0, 0);
    while let Some(offset) = text[from..].find('{') {
        let start = from + offset;
        match object_end(text, start) {
            Some(end) => {
                spans.push((start, end));
                from = end;
            }
            None if unclosed < MAX_BRACE_CANDIDATES => {
                unclosed += 1;
                from = start + 1;
            }
            None => break,
        }
    }
    spans
}

/// The object that closes `text` and that `is_answer` accepts: where it
/// starts — or the code fence wrapping it, when one does — and its text.
///
/// The object is the one that ends where the text ends (but for whitespace
/// and a closing fence), found from the right, so a brace in the prose
/// before it never hides it. At most [`MAX_BRACE_CANDIDATES`] braces are
/// tried.
pub(crate) fn closing_object(
    text: &str,
    is_answer: impl Fn(&str) -> bool,
) -> Option<(usize, &str)> {
    let trimmed = text.trim_end();
    let (body, fenced) = match trimmed.strip_suffix("```") {
        Some(rest) => (rest.trim_end(), true),
        None => (trimmed, false),
    };
    let mut candidates = body;
    let mut tried = 0;
    while let Some(start) = candidates.rfind('{') {
        tried += 1;
        if tried > MAX_BRACE_CANDIDATES {
            return None;
        }
        candidates = &body[..start];
        if object_end(body, start) != Some(body.len()) || !is_answer(&body[start..]) {
            continue;
        }
        if !fenced {
            return Some((start, &body[start..]));
        }
        let before = body[..start].trim_end();
        let opening = before.strip_suffix("```").or_else(|| {
            let split = before.len().checked_sub("```json".len())?;
            before
                .get(split..)
                .filter(|tail| tail.eq_ignore_ascii_case("```json"))
                .map(|_| &before[..split])
        });
        // A closing fence that does not wrap the object belongs to
        // something else.
        return opening.map(|opening| (opening.len(), &body[start..]));
    }
    None
}

/// The end (exclusive) of the JSON object opening at `start`: string- and
/// escape-aware brace matching. `None` when it never closes.
fn object_end(text: &str, start: usize) -> Option<usize> {
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (index, ch) in text[start..].char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod json_object_tests {
    use super::json_objects;
    use serde_json::json;

    #[test]
    fn braces_in_strings_stay_inside_their_object() {
        let objects = json_objects(
            "Report.\n{\"choice\":\"findings\",\"additional_data\":{\"note\":\"a } and a {\",\
             \"nested\":{\"choice\":\"inner\"}}}",
        );
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0]["choice"], json!("findings"));
        assert_eq!(objects[0]["additional_data"]["note"], json!("a } and a {"));
    }
}
