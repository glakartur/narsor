//! What an execution waits on — the pending question's form and what the
//! conductor adds to it — printed after `show`, `run`, `input`, `resume` and
//! `recover`.

use serde_json::Value;

use crate::api::{ExecutionStatusDto, ExecutionSummary, ProblemKindDto};
use crate::domain::without_unsafe_chars;
use narsor_protocol::api::{
    ENTRY_LIMIT_CONTINUE, ENTRY_LIMIT_DECISION_FIELD, ENTRY_LIMIT_RESET_COUNTER,
};

/// Prints the pending question's full form ([`print_pending_input`]) —
/// fields, types, allowed values, output labels, not just its instruction
/// text — that [`run`]/[`resume`]/[`input`]/[`recover`] always printed when
/// a pass stops on a question, if `summary` says the execution is
/// `InputNeeded` right now; then what the conductor adds to it
/// ([`print_conductor_matters`]).
///
/// `ExecutionSummary::pending_input` (added to `GET /executions` while
/// migrating the TUI off `Arc<Engine>`, U7) already carries this whole form
/// — an earlier version of this function predated that field and had to
/// settle for re-reading the event log just to recover the bare instruction
/// text from `HumanInputRequested`, the one piece it could get any other
/// way. Callers already fetch `summary` from `GET /executions` before
/// calling this, so reusing it here costs nothing further. An
/// operator-requested pause (`WorkflowPaused`, `Engine::pause_now`) has no
/// pending question and `pending_input` is `None` for it, matching
/// `engine.pending_input()` returning `None` for that case before this
/// unit.
///
/// [`run`]: super::run
/// [`resume`]: super::resume
/// [`input`]: super::input
/// [`recover`]: super::recover
pub(super) fn print_open_matters(summary: &ExecutionSummary) {
    if summary.status == ExecutionStatusDto::InputNeeded {
        if let Some(pending) = &summary.pending_input {
            print_pending_input(pending);
        }
    }
    print_conductor_matters(summary);
}

/// What the conductor adds to `summary`, as the TUI shows it: the failure
/// a `Failed` execution is a problem on (F3), the summary of the gate it
/// waits on (R30), and what the conductor is working on right now (R11).
///
/// A problem input ([`ProblemKindDto::Output`]) is not repeated here: the
/// pending input's form already says it.
pub(super) fn print_conductor_matters(summary: &ExecutionSummary) {
    if let Some(problem) = summary
        .problem
        .as_ref()
        .filter(|problem| problem.kind == ProblemKindDto::Failure)
    {
        println!("Problem ({}): {}", problem.kind.name(), problem.reason);
        if let Some(episode) = &problem.episode {
            println!("  Failure: {episode}");
        }
        println!(
            "  Recover with: narsor recover {} [--comment TEXT]",
            summary.id
        );
    }
    if let Some(annotation) = &summary.annotation {
        println!("Conductor's summary: {}", annotation.summary);
        if let Some(recommendation) = &annotation.recommendation {
            println!("  Recommends: {recommendation}");
        }
    }
    if let Some(activity) = &summary.conductor {
        println!(
            "Conductor working: {} ({}, since {})",
            activity.purpose.name(),
            activity.trigger.name(),
            activity.started_at.to_rfc3339()
        );
    }
}

/// The question an execution is waiting on, as a form the operator can fill in.
pub(super) fn print_pending_input(pending: &impl serde::Serialize) {
    for line in pending_input_lines(pending) {
        println!("{line}");
    }
}

/// The lines [`print_pending_input`] prints.
/// The reason of the agent or script step before a gate and the values its
/// choice required (KTD11), from an already cleaned `pending`. A value may
/// span lines; each further line is indented under its name.
fn gate_context_lines(pending: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(reason) = pending["reason"].as_str() {
        lines.push(format!("  Reason: {}", without_unsafe_chars(reason)));
    }
    let required = pending["required_values"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if required.is_empty() {
        return lines;
    }
    lines.push("  Required values:".to_string());
    for value in &required {
        let mut text = value["value"].as_str().unwrap_or_default().lines();
        lines.push(format!(
            "    {} ({}): {}",
            value["name"].as_str().unwrap_or("?"),
            value["kind"].as_str().unwrap_or("?"),
            text.next().unwrap_or_default()
        ));
        lines.extend(text.map(|line| format!("      {line}")));
    }
    lines
}

pub(in crate::cli) fn pending_input_lines(pending: &impl serde::Serialize) -> Vec<String> {
    let mut lines = Vec::new();
    let Ok(mut pending) = serde_json::to_value(pending) else {
        lines.push("Human input expected: (unavailable)".to_string());
        return lines;
    };
    // The instruction can carry a step's reply and the problem an agent's
    // answer: text from an agent, never handed to a terminal as it is.
    narsor_protocol::display::without_unsafe_chars_json(&mut pending);
    lines.push(format!(
        "Human input expected: {}",
        pending["instruction"].as_str().unwrap_or("(unavailable)")
    ));
    lines.extend(gate_context_lines(&pending));
    for field in pending["fields"].as_array().into_iter().flatten() {
        let requirement = if field["required"].as_bool().unwrap_or(false) {
            "required"
        } else {
            "optional"
        };
        let options_list = field["options"].as_array();
        let options = if options_list.is_none_or(Vec::is_empty) {
            String::new()
        } else {
            format!(
                ", allowed values: {}",
                options_list
                    .into_iter()
                    .flatten()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let field_type = match field["type"].as_str().unwrap_or_default() {
            "text" => "Text",
            "select" => "Select",
            other => other,
        };
        lines.push(format!(
            "  --field {}=…  ({field_type}, {requirement}{options})",
            field["name"].as_str().unwrap_or("?")
        ));
    }
    if let Some(denials) = pending["permission"]["denials"].as_array() {
        for (index, denial) in denials.iter().enumerate() {
            let operation = serde_json::to_string(&denial["input"])
                .unwrap_or_else(|_| "(unavailable)".to_string());
            lines.push(format!(
                "  Permission denial {index}: {} {}",
                without_unsafe_chars(denial["tool_name"].as_str().unwrap_or("tool")),
                without_unsafe_chars(&operation),
            ));
            lines.push(format!(
                "    cwd: {}",
                without_unsafe_chars(denial["cwd"].as_str().unwrap_or("(unknown)")),
            ));
            if let Some(reason) = denial["reason"].as_str() {
                lines.push(format!("    reason: {}", without_unsafe_chars(reason)));
            }
        }
        lines.push(
            "  Decide with --permission deny, allow-exact, allow-group, or allow-pattern --pattern TEXT."
                .to_string(),
        );
    }
    let outputs = pending["outputs"].as_array().cloned().unwrap_or_default();
    let problem = &pending["problem"];
    if problem.is_null() {
        if !outputs.is_empty() {
            let labels = outputs
                .iter()
                .filter_map(|output| output.as_str().or_else(|| output["label"].as_str()))
                .collect::<Vec<_>>();
            lines.push(format!("  Output labels: {}", labels.join(", ")));
        }
        return lines;
    }

    // A problem: the step's output is what is asked for (KTD3).
    lines.push(format!(
        "  Problem: {}",
        problem["reason"].as_str().unwrap_or("(unknown)")
    ));
    for answer in problem["answers"].as_array().into_iter().flatten() {
        if let Some(tail) = answer["answer_tail"].as_str() {
            lines.push(format!(
                "  Agent answered ({}):",
                answer["action_execution_id"].as_str().unwrap_or("unknown")
            ));
            for line in tail.lines() {
                lines.push(format!("    | {line}"));
            }
        }
    }
    if let Some(limit) = pending["entry_limit"].as_object() {
        // The step's entry limit (R10): the operator lets it run again, or
        // chooses its output instead.
        lines.push(format!(
            "  Entry limit: step '{}' entered {}/{} times.",
            pending["state_id"].as_str().unwrap_or("?"),
            limit
                .get("entries")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            limit
                .get("max_entries")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        ));
        lines.push(format!(
            "  Decide with --field {ENTRY_LIMIT_DECISION_FIELD}={ENTRY_LIMIT_CONTINUE} (run the \
             step once more) or --field {ENTRY_LIMIT_DECISION_FIELD}={ENTRY_LIMIT_RESET_COUNTER} \
             (start its counter again; the limit no longer guards the loop until it is reached \
             again)."
        ));
        lines.push("  Or choose an output with --label and the keys that label needs:".to_string());
    } else {
        lines.push("  Resolve with --label and the keys that label needs:".to_string());
    }
    for output in outputs {
        let label = output
            .as_str()
            .or_else(|| output["label"].as_str())
            .unwrap_or("?");
        let description = output["description"]
            .as_str()
            .map(|description| format!("  — {description}"))
            .unwrap_or_default();
        lines.push(format!("    --label {label}{description}"));
        if let Some(requirement) = problem["requirements"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|requirement| requirement["label"].as_str() == Some(label))
        {
            for key in requirement["artifacts"].as_array().into_iter().flatten() {
                if let Some(key) = key.as_str() {
                    lines.push(format!("        requires --artifact {key}=…"));
                }
            }
            for key in requirement["variables"].as_array().into_iter().flatten() {
                if let Some(key) = key.as_str() {
                    lines.push(format!("        requires --variable {key}=…"));
                }
            }
        }
    }
    lines
}
