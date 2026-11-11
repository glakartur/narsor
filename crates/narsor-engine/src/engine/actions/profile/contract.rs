//! What the profiling agent is told: the engine's data about the steps it
//! may profile (R2), and the answer it has to end with.

use serde_json::{json, Value};

use super::candidates::ProfileCandidate;
use crate::domain::{ProfileActionConfig, WorkspaceAccess};
use crate::engine::actions::session::RESUME_SESSION_PARAMETER;
use crate::engine::step_profile::{Setting, EFFORT_SCALE, MODEL_TIERS};

/// The answer object of a `profile` step.
const PROFILE_ANSWER_SHAPE: &str = "{\"entries\": [{\"step\": \"<step id>\", \"model\": \
\"<model tier>\", \"effort\": \"<effort>\", \"rationale\": \"<one line>\", \"confident\": true}], \
\"reason\": \"<one line>\"}";

/// The engine's data block: the steps the agent may profile, each with its
/// description, its defaults and its ceiling, then the model tiers and the
/// efforts. The author's prompt says where the work is described; this says
/// what can be chosen.
pub fn data_block(candidates: &[ProfileCandidate]) -> String {
    let mut text = String::from(
        "\n\n---\n\
You choose a model tier and an effort for the steps of this workflow listed \
below, from how hard their work is. Do not do the work of any step yourself, \
and do not change any files.\n",
    );
    if candidates.is_empty() {
        text.push_str("There are no steps to choose for: answer with an empty `entries` list.\n");
    } else {
        text.push_str("The steps (id — what it does; default; ceiling):\n");
        for candidate in candidates {
            text.push_str(&candidate_line(candidate));
        }
    }
    text.push_str(&format!(
        "Model tiers, cheapest first: {}.\nEfforts, cheapest first: {}.\n",
        quoted_list(&MODEL_TIERS),
        quoted_list(&EFFORT_SCALE)
    ));
    text
}

/// One step of the data block.
fn candidate_line(candidate: &ProfileCandidate) -> String {
    let mut line = format!("- `{}`", candidate.step_id);
    if let Some(description) = &candidate.description {
        line.push_str(" — ");
        line.push_str(&description.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    line.push_str(&format!("; default: {}", setting_text(&candidate.default)));
    match &candidate.ceiling {
        Some(ceiling) => line.push_str(&format!(
            "; ceiling: {}",
            setting_text(&Setting {
                model: ceiling.model.clone().or(candidate.default.model.clone()),
                effort: ceiling.effort.clone().or(candidate.default.effort.clone()),
            })
        )),
        None => line.push_str("; no ceiling: a value above the default is lowered to it"),
    }
    line.push('\n');
    line
}

/// `model `x`, effort `y``, with the harness's own default for a value the
/// step does not set.
fn setting_text(setting: &Setting) -> String {
    let value = |value: &Option<String>| {
        value
            .as_deref()
            .map_or("the harness's default".to_string(), |value| {
                format!("`{value}`")
            })
    };
    format!(
        "model {}, effort {}",
        value(&setting.model),
        value(&setting.effort)
    )
}

fn quoted_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("`{value}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The answer the agent has to end with. `harness_note` is how the harness
/// wants it delivered; it closes the contract.
pub fn answer_contract(harness_note: Option<&str>) -> String {
    format!(
        "\n\n---\n\
When you are done, end your reply with a single JSON object on its own, and \
nothing after it:\n\
{PROFILE_ANSWER_SHAPE}\n\
Give one entry for each step whose model or effort should change; leave out a \
step that keeps its default. `step` is a step id from the list, `model` one of \
the model tiers and `effort` one of the efforts. `rationale` says why, in one \
line. Set `confident` to false when you are not sure: such an entry is not \
used. `reason` sums up the profile in one line. An empty `entries` list keeps \
every step on its default. Send the answer exactly once, when all your \
subagents have returned their results. Do not omit the JSON object: without it \
the workflow cannot continue.{note}",
        note = harness_note
            .map(|note| format!("\n{note}"))
            .unwrap_or_default(),
    )
}

/// The JSON Schema of the answer: one flat object, no `anyOf` at the top
/// level (a harness may hand it to its API as a tool's input schema). The
/// engine checks every entry itself; the schema only describes the shape.
pub fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "entries": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "step": { "type": "string" },
                        "model": { "type": "string", "enum": MODEL_TIERS },
                        "effort": { "type": "string", "enum": EFFORT_SCALE },
                        "rationale": { "type": "string" },
                        "confident": { "type": "boolean" },
                    },
                    "required": ["step", "rationale", "confident"],
                    "additionalProperties": false,
                },
            },
            "reason": { "type": "string" },
        },
        "required": ["entries"],
        "additionalProperties": false,
    })
}

/// What the engine asks a profiling agent whose answer had no usable
/// profile, in the same harness session, before the answer contract.
pub const PROFILE_REPROMPT: &str = "Your previous reply could not be used by the workflow, \
because it did not end with a valid step profile object. Do not do any further work and \
do not change any files. Reply only with the JSON object described below, with the \
profile you already decided on.";

/// The configuration of the engine's reprompt of a profiling agent (R13):
/// resumes `harness_session` on `harness`, read-only, asking only for the
/// profile object. `reason` says why the previous answer was rejected.
pub fn reprompt_config(
    original: &ProfileActionConfig,
    harness: &str,
    harness_session: &str,
    reason: &str,
) -> ProfileActionConfig {
    let mut config = original.clone();
    config.harness_step.harness = Some(harness.to_string());
    config.harness_step.workspace_access = Some(WorkspaceAccess::ReadOnly);
    config.prompt = format!("{PROFILE_REPROMPT}\n\nWhy it was rejected: {reason}").into();
    config.harness_step.parameters.insert(
        RESUME_SESSION_PARAMETER.to_string(),
        Value::String(harness_session.to_string()),
    );
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, ceiling: Option<Setting>) -> ProfileCandidate {
        ProfileCandidate {
            step_id: id.into(),
            description: Some("Does\n  the work.".into()),
            default: Setting::new(Some("medium"), Some("low")),
            ceiling,
        }
    }

    #[test]
    fn the_data_block_lists_steps_defaults_ceilings_tiers_and_efforts() {
        let block = data_block(&[
            candidate("work", Some(Setting::new(Some("large"), None))),
            candidate("review", None),
        ]);
        assert!(
            block.contains(
                "- `work` — Does the work.; default: model `medium`, effort `low`; \
                 ceiling: model `large`, effort `low`"
            ),
            "{block}"
        );
        assert!(
            block.contains(
                "- `review` — Does the work.; default: model `medium`, effort `low`; no ceiling"
            ),
            "{block}"
        );
        assert!(block.contains("Model tiers, cheapest first: `small`, `medium`, `large`, `max`"));
        for effort in EFFORT_SCALE {
            assert!(block.contains(&format!("`{effort}`")), "{block}");
        }
    }

    #[test]
    fn an_empty_list_asks_for_empty_entries() {
        assert!(data_block(&[]).contains("answer with an empty `entries` list"));
    }

    #[test]
    fn the_schema_is_a_flat_object_without_a_choice() {
        let schema = answer_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("anyOf").is_none());
        assert!(schema["properties"].get("choice").is_none());
        assert_eq!(schema["required"], json!(["entries"]));
    }

    #[test]
    fn the_contract_has_no_choice_and_ends_with_the_harness_note() {
        let contract = answer_contract(Some("NOTE"));
        assert!(!contract.contains("choice"));
        assert!(contract.contains("\"entries\""));
        assert!(contract.ends_with("\nNOTE"));
    }
}
