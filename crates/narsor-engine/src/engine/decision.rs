//! Choosing the transition a finished action selects.
//!
//! `WORKFLOW_SCHEMA.md` §3.4 knows three decision providers. Two of them exist
//! in this version:
//!
//! * **`rules`** (the default, and what a state without a `decision` block
//!   gets): the label the action returned *is* the decision. There is no
//!   expression engine — D6 makes the mapping static on purpose.
//! * **`human`**: the execution suspends and a person picks one of the state's
//!   declared outputs.
//!
//! The third, **`agent`**, is refused with an explicit error. Treating it as
//! `rules` would be worse than failing: the workflow author asked for a
//! judgement the engine cannot make, and silently substituting the action's own
//! label would take a transition nobody chose.
//!
//! # Where label validation lives
//!
//! Also here, and nowhere else. Executors deliberately report whatever label
//! they observed ([`crate::engine::actions`]), because only the state knows
//! which labels are permitted. [`check_label`] is the single gate: it is what
//! turns a label of `weird` into a failure naming the permitted set, and it is
//! where D20 (an agent that has to be told to pick again) will hook in.

use crate::domain::{
    ActionResult, DecisionConfig, DecisionProviderType, OutputDefinition, OutputLabel, State,
    StateId,
};

/// Message shown to an operator asked to pick a transition, when the workflow
/// does not supply one.
pub const DEFAULT_HUMAN_DECISION_MESSAGE: &str = "Pick the transition to take from this state.";

/// What a decision provider concluded.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionRequest {
    /// The provider chose a label without leaving the process.
    Resolved(DecisionOutcome),

    /// A person has to choose; the execution suspends until they do.
    AwaitHuman {
        /// The prompt to show them.
        message: String,
    },
}

/// A decision that was reached.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOutcome {
    /// The label selected, still to be checked with [`check_label`].
    pub label: OutputLabel,

    /// Who selected it.
    pub provider: DecisionProviderType,

    /// Why, when the provider can say.
    pub reasoning: Option<String>,
}

impl DecisionOutcome {
    /// The provider name as the event log and `WORKFLOW_SCHEMA.md` spell it.
    pub fn provider_name(&self) -> &'static str {
        provider_name(&self.provider)
    }
}

/// The schema spelling of a provider type.
pub fn provider_name(provider: &DecisionProviderType) -> &'static str {
    match provider {
        DecisionProviderType::Rules => "rules",
        DecisionProviderType::Agent => "agent",
        DecisionProviderType::Human => "human",
        DecisionProviderType::Conductor => "conductor",
    }
}

/// Everything that can stop a decision from being usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecisionError {
    /// The workflow asks for an agent decision, which this version cannot make.
    #[error(
        "state '{state}' uses an agent decision provider, which is not available \
         in this version of narsor; use 'rules' or 'human' instead"
    )]
    AgentProviderUnavailable {
        /// The state that asked for it.
        state: StateId,
    },

    /// The action returned a label the state does not declare.
    #[error(
        "action of state '{state}' returned output label '{label}', which the state \
         does not declare; permitted labels are: {}",
        permitted.join(", ")
    )]
    LabelNotDeclared {
        /// The state whose action ran.
        state: StateId,
        /// The label that came back.
        label: OutputLabel,
        /// The labels the state declares in `outputs`.
        permitted: Vec<OutputLabel>,
    },

    /// The label is declared but the state has no transition for it.
    ///
    /// Static validation (§5.1) rules this out before an execution starts; it
    /// is still checked here because the engine must never guess where to go.
    #[error("state '{state}' declares output '{label}' but has no transition for it")]
    NoTransition {
        /// The state whose action ran.
        state: StateId,
        /// The label with nowhere to go.
        label: OutputLabel,
    },
}

/// The labels a state declares, in declaration order.
pub fn permitted_labels(state: &State) -> Vec<OutputLabel> {
    state
        .outputs
        .iter()
        .map(|output| output.label.clone())
        .collect()
}

/// The outputs a state declares, descriptions included, in declaration order.
///
/// The same set as [`permitted_labels`], for the places that offer the labels
/// to someone choosing between them and should say what each one means.
pub fn permitted_outputs(state: &State) -> Vec<OutputDefinition> {
    state.outputs.clone()
}

/// Asks the state's decision provider which label the transition follows.
///
/// `result` is the action's own outcome, which the `rules` provider reads and
/// the others only take as context.
pub fn decide(state: &State, result: &ActionResult) -> Result<DecisionRequest, DecisionError> {
    match &state.decision {
        None | Some(DecisionConfig::Rules) => Ok(DecisionRequest::Resolved(DecisionOutcome {
            label: result.output_label.clone(),
            provider: DecisionProviderType::Rules,
            reasoning: None,
        })),

        Some(DecisionConfig::Human(config)) => Ok(DecisionRequest::AwaitHuman {
            message: config
                .message
                .clone()
                .unwrap_or_else(|| DEFAULT_HUMAN_DECISION_MESSAGE.to_string()),
        }),

        Some(DecisionConfig::Agent(_)) => Err(DecisionError::AgentProviderUnavailable {
            state: state.id.clone(),
        }),
    }
}

/// Checks a label against the state and returns the state it transitions to.
pub fn check_label<'a>(state: &'a State, label: &str) -> Result<&'a StateId, DecisionError> {
    let permitted = permitted_labels(state);

    if !permitted.iter().any(|declared| declared == label) {
        return Err(DecisionError::LabelNotDeclared {
            state: state.id.clone(),
            label: label.to_string(),
            permitted,
        });
    }

    state
        .transitions
        .get(label)
        .ok_or_else(|| DecisionError::NoTransition {
            state: state.id.clone(),
            label: label.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentDecisionConfig, HumanDecisionConfig, OutputDefinition};
    use std::collections::HashMap;

    fn state_with(decision: Option<DecisionConfig>) -> State {
        let mut transitions = HashMap::new();
        transitions.insert("success".to_string(), "next".to_string());

        State {
            id: "check".to_string(),
            description: None,
            action: None,
            outputs: vec![
                OutputDefinition {
                    label: "success".to_string(),
                    description: None,
                    requires_human_approval: false,
                    resume_comment_field: None,
                },
                OutputDefinition {
                    label: "orphan".to_string(),
                    description: None,
                    requires_human_approval: false,
                    resume_comment_field: None,
                },
            ],
            transitions,
            decision,
            timeout_secs: None,
            retry_policy: None,
            terminal: false,
            terminal_status: None,
            allow_manual_override: false,
            image: None,
        }
    }

    fn result(label: &str) -> ActionResult {
        ActionResult {
            output_label: label.to_string(),
            data: serde_json::Value::Null,
            variables: serde_json::Map::new(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    #[test]
    fn rules_take_the_label_the_action_returned() {
        let state = state_with(None);
        let decision = decide(&state, &result("success")).expect("decide");
        assert_eq!(
            decision,
            DecisionRequest::Resolved(DecisionOutcome {
                label: "success".to_string(),
                provider: DecisionProviderType::Rules,
                reasoning: None,
            })
        );
    }

    #[test]
    fn a_human_provider_asks_for_a_person() {
        let state = state_with(Some(DecisionConfig::Human(HumanDecisionConfig {
            message: Some("choose".to_string()),
        })));
        assert_eq!(
            decide(&state, &result("success")).expect("decide"),
            DecisionRequest::AwaitHuman {
                message: "choose".to_string()
            }
        );
    }

    #[test]
    fn an_agent_provider_is_refused() {
        let state = state_with(Some(DecisionConfig::Agent(AgentDecisionConfig {
            harness: Some("claude-code".to_string()),
            prompt: Some("decide".to_string()),
        })));
        let error = decide(&state, &result("success")).expect_err("agent is unavailable");
        assert_eq!(
            error,
            DecisionError::AgentProviderUnavailable {
                state: "check".to_string()
            }
        );
        assert!(error.to_string().contains("not available in this version"));
    }

    #[test]
    fn an_undeclared_label_names_the_permitted_ones() {
        let state = state_with(None);
        let error = check_label(&state, "weird").expect_err("label is not declared");
        let message = error.to_string();
        assert!(message.contains("'weird'"), "{message}");
        assert!(message.contains("success, orphan"), "{message}");
    }

    #[test]
    fn a_declared_label_without_a_transition_is_refused() {
        let state = state_with(None);
        assert_eq!(
            check_label(&state, "orphan").expect_err("no transition"),
            DecisionError::NoTransition {
                state: "check".to_string(),
                label: "orphan".to_string(),
            }
        );
    }

    #[test]
    fn a_declared_label_resolves_to_its_target_state() {
        let state = state_with(None);
        assert_eq!(check_label(&state, "success").expect("target"), "next");
    }
}
