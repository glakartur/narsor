use super::*;
use narsor_protocol::chat::QuestionSource;

#[derive(Debug, Clone)]
pub(super) enum AgentResponse {
    Completed(ChoiceAnswer),
    AwaitingInput {
        question: String,
        options: Vec<String>,
    },
}

/// Turns an agent's optional menu into a response form. Without a menu the
/// reply is required free text; with one, the operator can either type a free
/// answer or pick an option, optionally adding a comment.
pub(super) fn agent_input_fields(options: Vec<String>) -> Vec<crate::domain::HumanInputField> {
    use crate::domain::{HumanInputField, HumanInputType};

    if options.is_empty() {
        return vec![HumanInputField {
            name: "answer".to_string(),
            field_type: HumanInputType::Text,
            required: true,
            description: Some("Answer for the agent.".to_string()),
            options,
        }];
    }

    vec![
        HumanInputField {
            name: "option".to_string(),
            field_type: HumanInputType::Select,
            required: false,
            description: Some("Optional choice proposed by the agent.".to_string()),
            options,
        },
        HumanInputField {
            name: "comment".to_string(),
            field_type: HumanInputType::Text,
            required: false,
            description: Some("Optional comment attached to the choice.".to_string()),
            options: Vec::new(),
        },
        HumanInputField {
            name: "answer".to_string(),
            field_type: HumanInputType::Text,
            required: false,
            description: Some("Free-form answer instead of a choice.".to_string()),
            options: Vec::new(),
        },
    ]
}

/// Extracts the output payload from whatever the agent said (D19/D20).
///
/// Accepts a bare JSON object, a fenced `json` code block, and a JSON object
/// trailing any amount of prose — the last object with a `choice` key wins,
/// because an agent that restates its answer restates it at the end.
///
/// Both failures are deliberate errors and not a default label:
///
/// * nothing in the answer parses as a JSON object;
/// * objects are there, but none carries a string `choice`.
pub fn extract_output(text: &str) -> Result<ChoiceAnswer, ActionError> {
    match extract_response(text)? {
        AgentResponse::Completed(output) => Ok(output),
        AgentResponse::AwaitingInput { .. } => Err(no_label_error(
            "the agent requested human input instead of producing an output label",
            text,
        )),
    }
}

/// Reads the agent's answer from a finished run: its `structured_output`
/// first, when the harness reported one that makes a usable answer, then the
/// text, as [`extract_response`] always did.
///
/// A `structured_output` that is unusable — no `choice` label and no
/// complete `awaiting_input` question — is not an error of its own: the text
/// is read instead, and only its failure is reported. A label the state does
/// not declare is not unusable here; like a label read from text, it is the
/// engine's to reject.
pub(super) fn read_response(run: &HarnessRun) -> Result<AgentResponse, ActionError> {
    if let Some(structured) = &run.structured_output {
        match structured_response(structured) {
            Ok(response) => return Ok(response),
            Err(error) => tracing::warn!(
                reason = %error.message,
                "the harness's structured output is not a usable answer; reading the text instead"
            ),
        }
    }
    extract_response(&run.text)
}

/// What kind of final answer `value` is, by the same rule [`read_response`]
/// uses: `None` for anything that is not an answer. A label the state does
/// not declare is still a final answer; the engine rejects it later.
pub(crate) fn final_answer_kind(value: &Value) -> Option<FinalAnswer> {
    match structured_response(value) {
        Ok(AgentResponse::Completed(_)) => Some(FinalAnswer::Choice),
        Ok(AgentResponse::AwaitingInput { .. }) => Some(FinalAnswer::AwaitingInput),
        Err(_) => None,
    }
}

/// The final answer a harness's text ends with, and what kind it is: the
/// output-contract object that closes the text (the contract says "nothing
/// after it"), when it makes a usable answer.
///
/// Stricter than [`extract_response`] on purpose: a harness ends the step on
/// this, so an example object quoted in the middle of a message must not.
pub(crate) fn closing_answer(text: &str) -> Option<(Value, FinalAnswer)> {
    let (_, object) = final_contract(text)?;
    let value = serde_json::from_str::<Value>(object).ok()?;
    let kind = final_answer_kind(&value)?;
    Some((value, kind))
}

/// Reads an answer from a harness's `structured_output` value, by the same
/// rules as an object found in the text.
fn structured_response(value: &Value) -> Result<AgentResponse, ActionError> {
    let object = value
        .as_object()
        .filter(|object| is_answer_object(object))
        .ok_or_else(|| {
            no_label_error(
                "the structured output has neither a string 'choice' nor an 'awaiting_input' status",
                &value.to_string(),
            )
        })?;
    response_from_object(object).map_err(|reason| no_label_error(reason, &value.to_string()))
}

/// Writes the agent's question to the conversation as a narsor entry, when
/// the accepted answer asks one.
///
/// The executor is the only producer of an output-contract question: it
/// knows which answer it accepted — the structured output or the text — so
/// the conversation shows exactly that question, once.
pub(super) fn record_question(conversation: &ConversationSink, response: &AgentResponse) {
    let AgentResponse::AwaitingInput { question, options } = response else {
        return;
    };
    let entry = serde_json::json!({
        "event": "agent_question",
        "question": question,
        "options": options,
    })
    .to_string();
    conversation.append_narsor(
        &entry,
        &[ChatElement::AgentQuestion {
            question: question.clone(),
            options: options.clone(),
            source: QuestionSource::OutputContract,
            call_id: None,
        }],
    );
}

/// Extracts either a normal label or a request that pauses the same agent
/// action. The last matching object wins, following the normal output rule.
fn extract_response(text: &str) -> Result<AgentResponse, ActionError> {
    let objects = json_objects(text);

    if objects.is_empty() {
        return Err(no_label_error(
            "the agent's answer contains no JSON object at all",
            text,
        ));
    }

    let Some(object) = objects
        .iter()
        .rev()
        .find_map(|value| value.as_object().filter(|object| is_answer_object(object)))
    else {
        return Err(no_label_error(
            "the agent's answer contains JSON, but no object with a string 'choice' key",
            text,
        ));
    };

    response_from_object(object).map_err(|reason| no_label_error(reason, text))
}

/// Whether `object` is an answer at all: a question, or a string `choice`.
fn is_answer_object(object: &Map<String, Value>) -> bool {
    is_question(object) || answer::has_choice(object)
}

/// Whether `object` asks a person before the task continues.
fn is_question(object: &Map<String, Value>) -> bool {
    object.get("status").and_then(Value::as_str) == Some(AWAITING_INPUT)
}

/// Turns an answer object into the response it stands for. An error is the
/// reason the object is unusable; the caller quotes the answer it came
/// from (the agent's text, or the structured output) with it.
fn response_from_object(object: &Map<String, Value>) -> Result<AgentResponse, &'static str> {
    if is_question(object) {
        return question_from_object(object);
    }
    answer::read_choice_answer(object)
        .expect("the object was selected for having a choice")
        .map(AgentResponse::Completed)
}

/// Reads an `awaiting_input` object: its question and its optional menu.
fn question_from_object(object: &Map<String, Value>) -> Result<AgentResponse, &'static str> {
    let Some(question) = object.get("question").and_then(Value::as_str) else {
        return Err("the agent requested human input without a string 'question'");
    };
    if question.trim().is_empty() {
        return Err("the agent requested human input with an empty question");
    }
    let options = match object.get("options") {
        None => Vec::new(),
        Some(Value::Array(options)) => options
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .filter(|options| {
                !options.is_empty() && options.iter().all(|option| !option.trim().is_empty())
            })
            .map(|options| options.into_iter().map(ToString::to_string).collect())
            .ok_or("the agent's input options must be a non-empty array of non-empty strings")?,
        Some(_) => return Err("the agent's input options must be an array of strings"),
    };
    Ok(AgentResponse::AwaitingInput {
        question: question.to_string(),
        options,
    })
}

/// How much of the agent's answer an extraction failure quotes back.
const FAILED_EXTRACTION_QUOTE_BYTES: usize = 2 * 1024;

/// No label could be extracted — and none is invented.
fn no_label_error(reason: &str, text: &str) -> ActionError {
    ActionError {
        message: format!(
            "no output label could be extracted from the agent's answer: {reason}. \
             The agent must end with a JSON object carrying a 'choice' key \
             (docs/dev/workflow-schema.md §3.3.A)"
        ),
        error_type: Some(MISSING_OUTPUT_LABEL.to_string()),
        details: Some(serde_json::json!({
            "answer_tail": super::super::harness::tail(text, FAILED_EXTRACTION_QUOTE_BYTES),
        })),
    }
}

/// Adds the harness and its session id to the details of an extraction
/// failure, so the engine can resume that session to ask for the label again.
pub(super) fn record_session_in_details(
    error: &mut ActionError,
    harness: &str,
    session_id: Option<&str>,
) {
    let details = error
        .details
        .get_or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(details) = details {
        details.insert("harness".to_string(), Value::String(harness.to_string()));
        details.insert(
            "harness_session_id".to_string(),
            session_id.map_or(Value::Null, |id| Value::String(id.to_string())),
        );
    }
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
fn top_level_objects(text: &str) -> Vec<(usize, usize)> {
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

/// Builds the action result of a successful run.
pub(super) fn build_result(
    answer: ChoiceAnswer,
    session: &AgentSession,
    run: &HarnessRun,
    finished: &FinishedRun,
) -> ActionResult {
    let reply = run_reply(run).or_else(|| contract_reply(&answer.reason, &answer.additional_data));
    let session_record = serde_json::to_value(session).unwrap_or(Value::Null);

    let mut result = answer.into_result();
    result.data = serde_json::json!({ SESSION_DATA_KEY: session_record });
    if let Some(session_id) = &run.session_id {
        // Never over an agent's own variable of that name: the agent's context
        // is the agent's to write (D10 keeps the engine the only writer, and
        // the engine writes what it is given).
        result
            .variables
            .entry(SESSION_ID_VARIABLE.to_string())
            .or_insert_with(|| Value::String(session_id.clone()));
    }
    result.exit_code = Some(finished.exit_code);
    result.stdout = truncate(&run.text);
    result.stderr = truncate(&finished.stderr);
    result.reply = reply;
    result
}

/// The reply of an agent that answered with the output-contract object
/// alone (KTD17): its `reason`, and below it its `additional_data` as
/// pretty-printed JSON. Without it, a step whose findings live only in the
/// object hands nothing to `{{ steps.NAME.reply }}`, and the step reading
/// it fails. `None` when both are empty.
fn contract_reply(reason: &str, additional_data: &Map<String, Value>) -> Option<String> {
    let mut parts = Vec::new();
    if !reason.is_empty() {
        parts.push(reason.to_string());
    }
    if !additional_data.is_empty() {
        if let Ok(json) = serde_json::to_string_pretty(additional_data) {
            parts.push(json);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(truncate(&parts.join("\n\n")))
}

/// Adds the attempt's reply to the details of an extraction failure, so a
/// format reprompt can keep it instead of its own text (R3).
pub(super) fn record_reply_in_details(error: &mut ActionError, run: &HarnessRun) {
    let Some(reply) = run_reply(run) else {
        return;
    };
    if let Some(Value::Object(details)) = error.details.as_mut() {
        details.insert("reply".to_string(), Value::String(reply));
    }
}

/// The step's reply of `run`: the reply its harness read, or else its final
/// text, without the output-contract object (R3).
pub(crate) fn run_reply(run: &HarnessRun) -> Option<String> {
    clean_reply(run.reply.as_deref().unwrap_or(&run.text))
}

/// The agent's reply: its text without the output-contract object, and
/// without the code fence around that object (R3). `None` when nothing else
/// is left.
fn clean_reply(text: &str) -> Option<String> {
    let reply = match final_contract_start(text) {
        Some(cut) => text[..cut].to_string(),
        None => {
            let mut reply = text.to_string();
            if let Some((begin, end)) = last_contract_span(text) {
                reply.replace_range(begin..end, "");
            }
            reply
        }
    };
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate(trimmed))
    }
}

/// Where the text's closing output-contract object starts — or the code
/// fence wrapping it, when one does.
///
/// The object is the one that ends where the text ends (but for whitespace
/// and a closing fence), found from the right, so a brace in the prose
/// before it never hides it. At most [`MAX_BRACE_CANDIDATES`] braces are
/// tried.
fn final_contract_start(text: &str) -> Option<usize> {
    final_contract(text).map(|(cut, _)| cut)
}

/// [`final_contract_start`], with the text of the closing object as well.
fn final_contract(text: &str) -> Option<(usize, &str)> {
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
        if object_end(body, start) != Some(body.len()) || !is_contract(&body[start..]) {
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

/// Whether `object` is an output-contract object: an answer or a question.
fn is_contract(object: &str) -> bool {
    serde_json::from_str::<Value>(object)
        .ok()
        .and_then(|value| value.as_object().map(is_answer_object))
        .unwrap_or(false)
}

/// The last output-contract object among the text's top-level objects, for
/// a text that goes on after it.
fn last_contract_span(text: &str) -> Option<(usize, usize)> {
    top_level_objects(text)
        .into_iter()
        .rev()
        .find(|&(start, end)| is_contract(&text[start..end]))
}

#[cfg(test)]
mod json_object_tests {
    use super::{extract_response, json_objects, AgentResponse};
    use serde_json::json;

    fn label_of(text: &str) -> String {
        match extract_response(text).expect("a response") {
            AgentResponse::Completed(answer) => answer.choice,
            AgentResponse::AwaitingInput { .. } => panic!("not an output"),
        }
    }

    #[test]
    fn an_unbalanced_brace_in_the_prose_does_not_hide_the_label() {
        assert_eq!(
            label_of("The parser misses a `{` on line 3.\n{\"choice\":\"findings\"}"),
            "findings"
        );
        assert_eq!(
            label_of("Open { and a \"quote.\n```json\n{\"choice\":\"clean\"}\n```"),
            "clean"
        );
    }

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

    #[test]
    fn a_text_full_of_unclosed_braces_is_read_in_linear_time() {
        let text = format!("{}\n{{\"choice\":\"findings\"}}", "{".repeat(200_000));
        let started = std::time::Instant::now();
        let _ = json_objects(&text);
        let _ = super::clean_reply(&text);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "stray braces must not make the scan quadratic: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_last_object_still_wins() {
        assert_eq!(
            label_of("{\"choice\":\"first\"} then { and {\"choice\":\"second\"}"),
            "second"
        );
    }
}

#[cfg(test)]
mod reply_tests {
    use super::{clean_reply, contract_reply};
    use crate::engine::actions::script::{MAX_CAPTURED_OUTPUT_BYTES, TRUNCATION_MARKER};

    #[test]
    fn a_long_reply_keeps_its_beginning_and_is_marked_cut() {
        let text = format!("{}\n{{\"choice\":\"findings\"}}", "a".repeat(70 * 1024));
        let reply = clean_reply(&text).expect("a reply");
        assert!(reply.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            reply.len(),
            MAX_CAPTURED_OUTPUT_BYTES + TRUNCATION_MARKER.len()
        );
        assert!(reply.starts_with("aaa"));
    }

    #[test]
    fn reply_removes_the_contract_object_and_json_fence() {
        assert_eq!(
            clean_reply("Report here.\n```json\n{\"choice\":\"findings\"}\n```"),
            Some("Report here.".to_string())
        );
    }

    #[test]
    fn contract_only_text_has_no_reply() {
        assert_eq!(clean_reply("{\"choice\":\"clean\"}"), None);
    }

    #[test]
    fn an_unbalanced_brace_in_the_prose_does_not_hide_the_contract() {
        assert_eq!(
            clean_reply("The parser misses a `{` on line 3.\n{\"choice\":\"findings\"}"),
            Some("The parser misses a `{` on line 3.".to_string())
        );
    }

    #[test]
    fn braces_inside_the_contract_are_part_of_it() {
        assert_eq!(
            clean_reply(
                "Report.\n{\"choice\":\"findings\",\"additional_data\":{\"note\":\"a } and a {\",\
                 \"nested\":{\"choice\":\"inner\"}}}"
            ),
            Some("Report.".to_string())
        );
    }

    #[test]
    fn a_fenced_json_block_goes_with_the_contract() {
        assert_eq!(
            clean_reply("Report.\n\n```JSON\n{\"choice\":\"clean\"}\n```\n"),
            Some("Report.".to_string())
        );
        assert_eq!(
            clean_reply("Report.\n```\n{\"choice\":\"clean\"}\n```"),
            Some("Report.".to_string())
        );
    }

    #[test]
    fn an_earlier_code_block_stays_in_the_reply() {
        let text = "Fix it like this:\n```rust\nfn main() { println!(\"{}\", 1); }\n```\n\
                    Done.\n{\"choice\":\"findings\"}";
        assert_eq!(
            clean_reply(text),
            Some(
                "Fix it like this:\n```rust\nfn main() { println!(\"{}\", 1); }\n```\nDone."
                    .to_string()
            )
        );
    }

    #[test]
    fn a_reply_that_is_a_code_block_keeps_its_fences() {
        let text = "```text\nreport\n```\n{\"choice\":\"findings\"}";
        assert_eq!(clean_reply(text), Some("```text\nreport\n```".to_string()));
    }

    #[test]
    fn a_contract_followed_by_prose_is_still_removed() {
        assert_eq!(
            clean_reply("Report.\n{\"choice\":\"findings\"}\nThanks."),
            Some("Report.\n\nThanks.".to_string())
        );
    }

    #[test]
    fn the_reason_and_the_additional_data_stand_in_for_the_reply_of_a_contract_alone() {
        let data = serde_json::json!({ "findings": [{ "title": "Whitespace aborts startup" }] });
        let reply = contract_reply("one finding", data.as_object().expect("an object"))
            .expect("a reply from the contract");
        assert!(reply.starts_with("one finding\n\n"), "{reply}");
        assert!(reply.contains("Whitespace aborts startup"), "{reply}");
    }

    #[test]
    fn the_reason_alone_is_the_reply() {
        assert_eq!(
            contract_reply("all checks", &serde_json::Map::new()),
            Some("all checks".to_string())
        );
    }

    #[test]
    fn an_empty_contract_gives_no_reply() {
        assert_eq!(contract_reply("", &serde_json::Map::new()), None);
    }
}
