//! The action types, and what each of them can do.

use super::*;

/// The action types an executor can be registered for.
///
/// A typed key rather than a string, so that registering an executor for a
/// misspelled action type cannot compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// `AgentAction`.
    Agent,
    /// `ScriptAction`.
    Script,
    /// `HumanAction`.
    Human,
    /// `WaitAction` — no executor in this version.
    Wait,
    /// `DecisionAction` — no executor in this version.
    Decision,
    /// `ProfileAction`: an agent that proposes the step profile.
    Profile,
}

impl ActionKind {
    /// The kind of an action configuration.
    pub fn of(action: &ActionConfig) -> Self {
        match action {
            ActionConfig::Agent(_) => Self::Agent,
            ActionConfig::Script(_) => Self::Script,
            ActionConfig::Human(_) => Self::Human,
            ActionConfig::Wait(_) => Self::Wait,
            ActionConfig::Decision(_) => Self::Decision,
            ActionConfig::Profile(_) => Self::Profile,
        }
    }

    /// The name this action type has in the YAML contract and in events.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Script => "script",
            Self::Human => "human",
            Self::Wait => "wait",
            Self::Decision => "decision",
            Self::Profile => "profile",
        }
    }

    // The capabilities below each spell out every kind rather than catch
    // the rest with `_`, so that a new action kind has to decide what it
    // can do instead of inheriting it.

    /// Whether the action runs an agent session on a harness: it gets the
    /// step's history, a pinned harness, the step profile and a waiting
    /// answer, keeps a session a later step or gate may resume, and counts
    /// its entries against `max_entries`.
    pub fn runs_harness(self) -> bool {
        match self {
            Self::Agent | Self::Profile => true,
            Self::Script | Self::Human | Self::Wait | Self::Decision => false,
        }
    }

    /// What the engine's reprompt asks when an answer breaks the step
    /// contract — a label the state does not declare, a value outside its
    /// `options` — and is asked for again (KTD7). `None` for an action whose
    /// such answer fails the action instead.
    pub fn reprompt_text(self) -> Option<&'static str> {
        match self {
            Self::Agent => Some(agent::OUTPUT_REPROMPT),
            Self::Profile => Some(profile_action::PROFILE_REPROMPT),
            Self::Script | Self::Human | Self::Wait | Self::Decision => None,
        }
    }

    /// Whether an answer that breaks the step contract is rejected and
    /// asked for again ([`Self::reprompt_text`]) rather than failing the
    /// action.
    pub fn reprompts_rejected_output(self) -> bool {
        self.reprompt_text().is_some()
    }

    /// Whether the conductor may settle an output the reprompt did not
    /// repair, by choosing the step's label for it (F1, `rescue.rs`). A
    /// `profile` step has no label to choose: its unusable output goes to
    /// the operator.
    pub fn rescues_output_label(self) -> bool {
        match self {
            Self::Agent => true,
            Self::Profile | Self::Script | Self::Human | Self::Wait | Self::Decision => false,
        }
    }

    /// Whether an output the engine's reprompt did not repair counts as a
    /// failed attempt of the step, so that `retry_policy` runs the action
    /// again before the operator is asked (R13). An `agent` step goes to
    /// the conductor's rescue instead.
    pub fn retries_unrepaired_output(self) -> bool {
        match self {
            Self::Profile => true,
            Self::Agent | Self::Script | Self::Human | Self::Wait | Self::Decision => false,
        }
    }

    /// Whether the action answers with the step contract: a label, a
    /// reason and variables a gate after it shows.
    pub fn answers_with_step_contract(self) -> bool {
        match self {
            Self::Agent | Self::Script => true,
            Self::Profile | Self::Human | Self::Wait | Self::Decision => false,
        }
    }

    /// Whether the variables its result gives are held to the step's
    /// declarations: a step answering with the step contract, and a
    /// decision, whose answer the conductor may give with variables.
    pub fn holds_variables_to_declarations(self) -> bool {
        match self {
            Self::Agent | Self::Script | Self::Decision => true,
            Self::Profile | Self::Human | Self::Wait => false,
        }
    }
}

/// Whether `state` runs an action on a harness ([`ActionKind::runs_harness`]).
pub(crate) fn runs_harness(state: &State) -> bool {
    state
        .action
        .as_ref()
        .is_some_and(|action| ActionKind::of(action).runs_harness())
}
