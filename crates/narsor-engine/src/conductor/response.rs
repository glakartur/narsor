//! The conductor's structured answer, and what makes one usable (KTD6).
//!
//! The conductor has no tools to the API. It ends its reply with one JSON
//! object — a decision, the label and values that go with it, how sure it
//! is, why, what it proposes and what it tells the operator — and the
//! engine parses it, checks it and carries it out. This module is the
//! parsing and the checking; carrying it out is the caller's (U10–U12).
//!
//! # The wire shape
//!
//! ```json
//! {"decision": "choose_output", "label": "implemented",
//!  "artifacts": {}, "variables": {}, "confidence": "high",
//!  "rationale": "…", "message": "…", "proposals": []}
//! ```
//!
//! `decision` names a [`DecisionKind`] and decides which other fields are
//! required (see [`Decision`]). `confidence` defaults to `low`: a decision
//! the conductor does not declare itself sure of is never treated as sure.
//!
//! # What makes an answer unusable
//!
//! Every case below is a failure of the invocation, never a reason to ask
//! the conductor again (KTD6) — the one repeat of R24 is the actor's:
//!
//! * the reply holds no JSON object with a `decision` key;
//! * the object does not match the shape ([`ResponseError::Schema`]);
//! * the decision is not one the invocation's purpose allows
//!   ([`ResponseError::Disallowed`]) — a diagnosis of a failed step that
//!   chooses an output is rejected here, whatever it says (R23).
//!
//! Whether a chosen label is declared by the state, and whether the values
//! it requires are there, is not this module's judgement: that is KTD15's
//! validation, which turns a bad (a) into (c) rather than into a failure
//! (U10).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::domain::{ConductorPurpose, Confidence};
use crate::engine::actions::agent::json_objects;

/// The kinds of decision a conductor can answer with, as `decision` spells
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// (a): complete the step with this output label (F1).
    ChooseOutput,
    /// (b): ask the agent again, in the conductor's own words (F1).
    Reprompt,
    /// (c): make the step a problem for the operator (F1).
    Escalate,
    /// Try the failed step once more, restarted with a note (F3, R21).
    Retry,
    /// Only diagnose the failed step, and propose (F3, R22).
    Diagnose,
    /// Answer the operator's chat message (F4).
    Reply,
    /// Summarize a `human` gate, with a recommendation (R30).
    Annotate,
}

impl DecisionKind {
    /// The name as `decision` spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ChooseOutput => "choose_output",
            Self::Reprompt => "reprompt",
            Self::Escalate => "escalate",
            Self::Retry => "retry",
            Self::Diagnose => "diagnose",
            Self::Reply => "reply",
            Self::Annotate => "annotate",
        }
    }
}

/// The decisions an invocation for `purpose` may answer with.
///
/// * choosing an output (F1): (a), (b) or (c); after the conductor's own
///   reprompt failed, (b) is no longer allowed (R17);
/// * diagnosing a failure (F3): a diagnosis, or the one retry — never an
///   output, never a transition (R23); a transition can only be proposed;
/// * a chat reply: a message, with proposals;
/// * a gate annotation: a summary and a recommendation, never an answer
///   (R30).
pub fn allowed_decisions(purpose: ConductorPurpose) -> &'static [DecisionKind] {
    match purpose {
        ConductorPurpose::ChooseOutput => &[
            DecisionKind::ChooseOutput,
            DecisionKind::Reprompt,
            DecisionKind::Escalate,
        ],
        ConductorPurpose::ChooseOutputAfterReprompt => {
            &[DecisionKind::ChooseOutput, DecisionKind::Escalate]
        }
        ConductorPurpose::DiagnoseFailure => &[DecisionKind::Diagnose, DecisionKind::Retry],
        ConductorPurpose::ReplyToChat => &[DecisionKind::Reply],
        ConductorPurpose::SummarizeGate => &[DecisionKind::Annotate],
    }
}

/// Whether an invocation for `purpose` may propose actions for the
/// operator to confirm (R28): a diagnosis and a chat reply may, a label
/// rescue and an annotation decide or summarize and propose nothing.
pub fn proposals_allowed(purpose: ConductorPurpose) -> bool {
    matches!(
        purpose,
        ConductorPurpose::DiagnoseFailure | ConductorPurpose::ReplyToChat
    )
}

/// What the conductor decided, with the fields that decision requires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// (a): the step completes with `label`, as if the agent had given it.
    ChooseOutput {
        /// The output label chosen.
        label: String,
        /// The step's artifacts, name → path, filled in by the conductor.
        #[serde(default, deserialize_with = "nullable_paths")]
        artifacts: BTreeMap<String, String>,
        /// The step's `variables`, filled in by the conductor.
        #[serde(default, deserialize_with = "nullable_map")]
        variables: Map<String, Value>,
    },
    /// (b): the agent is asked again with `prompt`.
    Reprompt {
        /// What to ask the agent, in the conductor's words.
        prompt: String,
    },
    /// (c): the step becomes a problem. The label and values, when given,
    /// are only what the conductor proposes to the operator.
    Escalate {
        /// The label it would propose, if it has one.
        #[serde(default)]
        label: Option<String>,
        /// The artifacts it would propose, name → path.
        #[serde(default, deserialize_with = "nullable_paths")]
        artifacts: BTreeMap<String, String>,
        /// The `variables` it would propose.
        #[serde(default, deserialize_with = "nullable_map")]
        variables: Map<String, Value>,
    },
    /// The failed step is restarted once, with `note` for the agent (R21).
    Retry {
        /// The note the restarted step is given.
        note: String,
        /// What went wrong, as the conductor sees it.
        #[serde(default)]
        diagnosis: Option<String>,
    },
    /// The failed step is only diagnosed (R22).
    Diagnose {
        /// What went wrong, as the conductor sees it.
        diagnosis: String,
    },
    /// The operator's chat message is answered with the response's
    /// [`ConductorResponse::message`].
    Reply,
    /// A `human` gate is summarized for the operator (R30).
    Annotate {
        /// What the operator is asked to decide, in short.
        summary: String,
        /// What the conductor would answer, if it has a view.
        #[serde(default)]
        recommendation: Option<String>,
    },
}

impl Decision {
    /// The kind of this decision.
    pub fn kind(&self) -> DecisionKind {
        match self {
            Self::ChooseOutput { .. } => DecisionKind::ChooseOutput,
            Self::Reprompt { .. } => DecisionKind::Reprompt,
            Self::Escalate { .. } => DecisionKind::Escalate,
            Self::Retry { .. } => DecisionKind::Retry,
            Self::Diagnose { .. } => DecisionKind::Diagnose,
            Self::Reply => DecisionKind::Reply,
            Self::Annotate { .. } => DecisionKind::Annotate,
        }
    }
}

/// The kinds of action the conductor can propose (R28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// Answer the pending input with `values` (and `label`, for a problem).
    AnswerInput,
    /// Tell the agent `text`: an answer to its pending question, or a note
    /// for its next attempt.
    MessageAgent,
    /// Restart the failed step, with `text` as its note.
    Retry,
    /// Take the transition of output `label`.
    Transition,
    /// Cancel the execution.
    Abort,
}

/// One action the conductor proposes; nothing happens until the operator
/// confirms it (R28).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedAction {
    /// What is proposed.
    pub kind: ProposalKind,
    /// A one-line description for the operator.
    #[serde(default)]
    pub summary: Option<String>,
    /// The output label, for `answer_input` and `transition`.
    #[serde(default)]
    pub label: Option<String>,
    /// The field values, for `answer_input`.
    #[serde(default, deserialize_with = "nullable_map")]
    pub values: Map<String, Value>,
    /// The text, for `message_agent` and `retry`.
    #[serde(default)]
    pub text: Option<String>,
}

impl ProposedAction {
    /// A chat line for the proposal, for one without a summary.
    pub fn describe(&self) -> String {
        match self.kind {
            ProposalKind::Retry => match &self.text {
                Some(note) => format!("Restart the failed step with the note: {note}"),
                None => "Restart the failed step".to_string(),
            },
            ProposalKind::Transition => format!(
                "Take the transition of output '{}'",
                self.label.as_deref().unwrap_or_default()
            ),
            ProposalKind::Abort => "Cancel the execution".to_string(),
            ProposalKind::MessageAgent => format!(
                "Tell the agent: {}",
                self.text.as_deref().unwrap_or_default()
            ),
            ProposalKind::AnswerInput => {
                let values = Value::Object(self.values.clone());
                match &self.label {
                    Some(label) => format!("Answer the pending input with '{label}', {values}"),
                    None => format!("Answer the pending input with {values}"),
                }
            }
        }
    }
}

/// The conductor's whole answer, parsed and checked for its purpose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConductorResponse {
    /// What it decided.
    #[serde(flatten)]
    pub decision: Decision,
    /// How sure it is; `low` unless it says `high` (KTD6).
    #[serde(default)]
    pub confidence: Confidence,
    /// Why.
    #[serde(default)]
    pub rationale: Option<String>,
    /// What it tells the operator in the chat.
    #[serde(default)]
    pub message: Option<String>,
    /// What it proposes, for the operator to confirm.
    #[serde(default)]
    pub proposals: Vec<ProposedAction>,
}

/// Why a conductor's answer cannot be used. Each one fails the invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseError {
    /// The reply has no JSON object with a `decision` key.
    NoResponse(String),
    /// The object does not have the shape of a response.
    Schema(String),
    /// The decision is not one the purpose allows.
    Disallowed {
        /// The decision given.
        decision: DecisionKind,
        /// The purpose of the invocation.
        purpose: ConductorPurpose,
    },
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoResponse(reason) => write!(f, "the conductor's answer is unusable: {reason}"),
            Self::Schema(reason) => write!(
                f,
                "the conductor's answer does not match the response schema: {reason}"
            ),
            Self::Disallowed { decision, purpose } => write!(
                f,
                "the conductor answered '{}', which is not allowed when asked to {} \
                 (allowed: {})",
                decision.name(),
                purpose.name(),
                allowed_decisions(*purpose)
                    .iter()
                    .map(DecisionKind::name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl std::error::Error for ResponseError {}

impl ConductorResponse {
    /// Parses the conductor's reply `text` for an invocation for `purpose`.
    ///
    /// The last JSON object with a `decision` key wins, the same rule an
    /// agent's output follows: a model that restates its answer restates it
    /// at the end.
    pub fn parse(text: &str, purpose: ConductorPurpose) -> Result<Self, ResponseError> {
        let objects = json_objects(text);
        if objects.is_empty() {
            return Err(ResponseError::NoResponse(
                "it contains no JSON object at all".to_string(),
            ));
        }
        let Some(object) = objects
            .into_iter()
            .rev()
            .find(|object| object.get("decision").is_some())
        else {
            return Err(ResponseError::NoResponse(
                "it contains JSON, but no object with a 'decision' key".to_string(),
            ));
        };
        Self::from_value(object, purpose)
    }

    /// Reads the answer of an invocation: its `structured_output`, when the
    /// harness reported one that is a usable response, else its reply
    /// `text`, as [`Self::parse`] reads it. Only the text's failure is
    /// reported: an unusable structured output is not an error of its own.
    pub fn read(
        structured_output: Option<Value>,
        text: &str,
        purpose: ConductorPurpose,
    ) -> Result<Self, ResponseError> {
        if let Some(structured) = structured_output {
            match Self::from_value(structured, purpose) {
                Ok(response) => return Ok(response),
                Err(error) => tracing::warn!(
                    %error,
                    "the conductor's structured output is not a usable answer; reading the text instead"
                ),
            }
        }
        Self::parse(text, purpose)
    }

    /// Reads and checks a response already in JSON — the parsed answer of an
    /// invocation, as [`crate::conductor::ConductorAnswer::response`] holds
    /// it, or a decision saved for later.
    pub fn from_value(value: Value, purpose: ConductorPurpose) -> Result<Self, ResponseError> {
        let response: Self = serde_json::from_value(value)
            .map_err(|error| ResponseError::Schema(error.to_string()))?;
        response.check(purpose)?;
        Ok(response)
    }

    /// The response as JSON, in the wire shape [`Self::from_value`] reads.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a conductor response always serializes")
    }

    /// The kind of its decision.
    pub fn kind(&self) -> DecisionKind {
        self.decision.kind()
    }

    /// What the schema alone cannot say: the decision is allowed for the
    /// purpose, required text is not blank, and what is proposed can be
    /// carried out.
    fn check(&self, purpose: ConductorPurpose) -> Result<(), ResponseError> {
        let kind = self.kind();
        if !allowed_decisions(purpose).contains(&kind) {
            return Err(ResponseError::Disallowed {
                decision: kind,
                purpose,
            });
        }

        match &self.decision {
            Decision::ChooseOutput { label, .. } => required_text("label", label)?,
            Decision::Escalate { .. } => {}
            Decision::Reprompt { prompt } => required_text("prompt", prompt)?,
            Decision::Retry { note, .. } => required_text("note", note)?,
            Decision::Diagnose { diagnosis } => required_text("diagnosis", diagnosis)?,
            Decision::Reply => {
                required_text("message", self.message.as_deref().unwrap_or_default())?;
            }
            Decision::Annotate { summary, .. } => required_text("summary", summary)?,
        }

        if !self.proposals.is_empty() && !proposals_allowed(purpose) {
            return Err(ResponseError::Schema(format!(
                "proposals are not allowed when asked to {}",
                purpose.name()
            )));
        }
        for proposal in &self.proposals {
            check_proposal(proposal)?;
        }
        Ok(())
    }
}

fn check_proposal(proposal: &ProposedAction) -> Result<(), ResponseError> {
    match proposal.kind {
        ProposalKind::MessageAgent => required_text(
            "proposals[].text of message_agent",
            proposal.text.as_deref().unwrap_or_default(),
        ),
        ProposalKind::Transition => required_text(
            "proposals[].label of transition",
            proposal.label.as_deref().unwrap_or_default(),
        ),
        ProposalKind::AnswerInput | ProposalKind::Retry | ProposalKind::Abort => Ok(()),
    }
}

fn required_text(field: &str, value: &str) -> Result<(), ResponseError> {
    if value.trim().is_empty() {
        return Err(ResponseError::Schema(format!(
            "'{field}' must not be empty"
        )));
    }
    Ok(())
}

/// A map that may also be written as `null`.
fn nullable_map<'de, D>(deserializer: D) -> Result<Map<String, Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Map<String, Value>>::deserialize(deserializer)?.unwrap_or_default())
}

/// Artifact paths by name, which may also be written as `null`.
fn nullable_paths<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<BTreeMap<String, String>>::deserialize(deserializer)?.unwrap_or_default())
}
