//! The `autopilot` context variable: who first receives a question an
//! automatic step's agent asks.

use std::collections::HashMap;

use serde_json::Value;

/// How many questions the conductor answers for one entry into a step; the
/// next one goes to the operator.
pub const CONDUCTOR_ANSWER_LIMIT: u32 = 5;

/// What the variable `autopilot` says about the questions of agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autopilot {
    /// Every question goes to the operator.
    Off,
    /// The conductor answers only what the task, the context or the
    /// repository settle.
    On,
    /// The conductor also takes a reasonable default and says what it
    /// assumed.
    BestEffort,
}

impl Autopilot {
    /// The value in `variables`. Unset, a boolean, or any word but the two
    /// modes is [`Autopilot::Off`].
    pub fn of(variables: &HashMap<String, Value>) -> Self {
        match variables.get("autopilot") {
            Some(Value::String(text)) => Self::from_name(text).unwrap_or(Self::Off),
            _ => Self::Off,
        }
    }

    /// The mode a request's payload names; `None` when it names none.
    pub fn of_payload(payload: &Value) -> Option<Self> {
        payload
            .get("autopilot")
            .and_then(Value::as_str)
            .and_then(Self::from_name)
            .filter(|mode| *mode != Self::Off)
    }

    /// The name as the variable spells it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::BestEffort => "best-effort",
        }
    }

    fn from_name(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "best-effort" => Some(Self::BestEffort),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with(value: Value) -> HashMap<String, Value> {
        HashMap::from([("autopilot".to_string(), value)])
    }

    #[test]
    fn only_the_three_words_name_a_mode() {
        assert_eq!(Autopilot::of(&HashMap::new()), Autopilot::Off);
        assert_eq!(Autopilot::of(&with(json!("off"))), Autopilot::Off);
        assert_eq!(Autopilot::of(&with(json!("on"))), Autopilot::On);
        assert_eq!(
            Autopilot::of(&with(json!(" Best-Effort "))),
            Autopilot::BestEffort
        );
    }

    #[test]
    fn a_boolean_or_an_unknown_word_is_off() {
        for value in [
            json!(true),
            json!(false),
            json!("true"),
            json!("maybe"),
            json!(1),
        ] {
            assert_eq!(
                Autopilot::of(&with(value.clone())),
                Autopilot::Off,
                "{value}"
            );
        }
    }

    #[test]
    fn the_payload_names_an_active_mode_or_none() {
        assert_eq!(
            Autopilot::of_payload(&json!({ "autopilot": "best-effort" })),
            Some(Autopilot::BestEffort)
        );
        assert_eq!(Autopilot::of_payload(&json!({ "autopilot": "off" })), None);
        assert_eq!(Autopilot::of_payload(&json!({})), None);
    }
}
