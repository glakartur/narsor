//! Fields a workflow file no longer has, removed from the pinned copy of an
//! execution's definition.
//!
//! An execution pins the author's YAML as it was at start
//! (`executions/{id}/definition.yaml`), and that copy is loaded with the same
//! strict loader as a workflow file. A field the schema later retired would
//! make an execution started on an older version unresumable. An author's
//! workflow file with a retired field is still refused with an "unknown field"
//! error: [`without_retired_fields`] is only for pinned copies.
//!
//! Retired fields:
//!
//! * `plan` on an agent action (`states.<id>.action.plan`), removed with the
//!   conductor's step profiling. A `profile` step does that work now.

use std::borrow::Cow;

use serde_yaml::{Mapping, Value};

/// The retired key of an agent action.
const RETIRED_AGENT_KEY: &str = "plan";

/// The pinned definition without the fields the schema retired.
///
/// Gives back the text unchanged when it has no retired field, or when it is
/// not YAML this function can read: the strict loader then reports the
/// problem as for any definition. Only `plan` in a `type: agent` action
/// mapping is removed; every other unknown field still fails to load.
pub fn without_retired_fields(yaml: &str) -> Cow<'_, str> {
    let Ok(mut document) = serde_yaml::from_str::<Value>(yaml) else {
        return Cow::Borrowed(yaml);
    };
    if !remove_retired_agent_keys(&mut document) {
        return Cow::Borrowed(yaml);
    }
    match serde_yaml::to_string(&document) {
        Ok(text) => Cow::Owned(text),
        Err(_) => Cow::Borrowed(yaml),
    }
}

/// Removes `plan` from every agent action under `states`. True if one went.
fn remove_retired_agent_keys(document: &mut Value) -> bool {
    let Some(states) = document.get_mut("states").and_then(Value::as_mapping_mut) else {
        return false;
    };
    let mut removed = false;
    for (_, state) in states.iter_mut() {
        let Some(action) = state.get_mut("action").and_then(Value::as_mapping_mut) else {
            continue;
        };
        if is_agent_action(action) && action.remove(RETIRED_AGENT_KEY).is_some() {
            removed = true;
        }
    }
    removed
}

fn is_agent_action(action: &Mapping) -> bool {
    action.get("type").and_then(Value::as_str) == Some("agent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::load_from_str;

    const PINNED_WITH_PLAN: &str = r#"
schema: v2
name: planned
version: 1
states:
  plan:
    action: { type: agent, prompt: x, plan: true }
    exit: { next: done }
  done:
    description: end
terminal_states:
  done: completed
"#;

    #[test]
    fn a_pinned_definition_with_the_retired_plan_field_loads() {
        let definition = load_from_str(&without_retired_fields(PINNED_WITH_PLAN))
            .expect("the retired `plan` field is removed from a pinned copy");

        assert!(definition.states.contains_key("plan"));
    }

    #[test]
    fn a_definition_without_retired_fields_is_given_back_unchanged() {
        let yaml = PINNED_WITH_PLAN.replace(", plan: true", "");

        assert!(matches!(without_retired_fields(&yaml), Cow::Borrowed(_)));
    }

    #[test]
    fn another_unknown_field_still_fails_to_load() {
        let yaml = PINNED_WITH_PLAN.replace("plan: true", "plan: true, planned: true");

        let error = load_from_str(&without_retired_fields(&yaml))
            .expect_err("only the retired field is removed");

        assert!(
            error.to_string().contains("unknown field `planned`"),
            "{error}"
        );
    }

    #[test]
    fn plan_is_kept_on_an_action_that_is_not_an_agent() {
        let yaml = PINNED_WITH_PLAN.replace(
            "{ type: agent, prompt: x, plan: true }",
            "{ type: profile, prompt: x, plan: true }",
        );

        assert!(matches!(without_retired_fields(&yaml), Cow::Borrowed(_)));
        let error = load_from_str(&without_retired_fields(&yaml))
            .expect_err("`plan` was never a field of a profile step");
        assert!(
            error.to_string().contains("unknown field `plan`"),
            "{error}"
        );
    }

    #[test]
    fn text_that_is_not_yaml_is_given_back_unchanged() {
        let yaml = "states: [unclosed";

        assert_eq!(without_retired_fields(yaml), yaml);
    }
}
