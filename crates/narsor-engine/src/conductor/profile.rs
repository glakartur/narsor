//! The conductor profiles the steps that have not started, once, after the
//! plan step.
//!
//! The engine asks when the plan step succeeds ([`ProfileRequest`]) and waits
//! for the answer at most [`PROFILE_WAIT`]: a conductor that is slow, fails or
//! is unsure leaves the steps on their defaults and never holds the task (R8).
//! What it chose is checked here against each step's ceiling and the
//! model tiers through [`step_profile::resolve`] and kept in the
//! conductor's store ([`ConductorStore::add_profile_entries`]); the engine
//! reads it when a step starts.
//!
//! A request above a ceiling is not a blocking request: the step starts on the
//! ceiling and the entry keeps what was asked for
//! ([`ProfileEntry::requested_over_ceiling`]), which the terminal interface
//! shows. The existing request mechanism suspends the task, so it is not used.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::conductor::actor::InvocationOutcome;
use crate::conductor::response::{ConductorResponse, Decision};
use crate::conductor::store::{ConductorStore, ProfileEntry, QueuedRequest};
use crate::domain::{ConductorPurpose, ConductorTrigger};
use crate::engine::step_profile::{self, ProfileInput, Setting};
use crate::persistence::Result;

/// The longest the engine waits for the conductor's profile before the next
/// step starts on its defaults.
pub const PROFILE_WAIT: Duration = Duration::from_secs(120);

/// One step that has not started, as the conductor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCandidate {
    /// The step.
    pub step_id: String,
    /// What it runs with without a profile.
    pub default: Setting,
    /// The most it may be raised to.
    pub ceiling: Option<Setting>,
}

/// The payload of a `ProfileSteps` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileRequest {
    /// The plan step that just succeeded.
    pub plan_step: String,
    /// The plan step's variables, among them the complexity assessment.
    pub variables: Value,
    /// The plan step's artifacts, name → path; the plan is read from there.
    pub artifacts: Value,
    /// The assessment, when the plan step gave a valid one.
    pub complexity: Option<Value>,
    /// The steps that have not started.
    pub steps: Vec<ProfileCandidate>,
}

impl ProfileRequest {
    /// The conductor request carrying this payload.
    pub fn into_queued(self) -> QueuedRequest {
        QueuedRequest::new(ConductorTrigger::Profile, ConductorPurpose::ProfileSteps)
            .with_payload(json!(self))
    }
}

/// What a profile request leaves in the store: the entries the conductor
/// chose that hold up; nothing for an answer that does not.
///
/// `started` are the steps that began after the request was made: the profile
/// is decided once, so they are left alone.
pub(super) fn conclude_profile(
    store: &ConductorStore,
    request: &QueuedRequest,
    outcome: &InvocationOutcome,
    started: &BTreeSet<String>,
) -> Result<Vec<ProfileEntry>> {
    let InvocationOutcome::Answered(answer) = outcome else {
        return Ok(Vec::new());
    };
    let Ok(ConductorResponse {
        decision: Decision::Profile { steps },
        ..
    }) = ConductorResponse::from_value(answer.response.clone(), ConductorPurpose::ProfileSteps)
    else {
        return Ok(Vec::new());
    };
    let Ok(asked) = serde_json::from_value::<ProfileRequest>(request.payload.clone()) else {
        return Ok(Vec::new());
    };
    store.add_profile_entries(entries_of(&asked, steps), started)
}

/// Checks the conductor's `steps` against `asked` and turns what holds up
/// into entries. The model and the effort are judged on their own: one that
/// cannot be used does not drop the other.
pub fn entries_of(
    asked: &ProfileRequest,
    steps: Vec<crate::conductor::response::ProfiledStep>,
) -> Vec<ProfileEntry> {
    let mut entries = Vec::new();
    for step in steps {
        if !step.confident {
            continue;
        }
        let Some(candidate) = asked.steps.iter().find(|c| c.step_id == step.step_id) else {
            continue;
        };
        let wanted = Setting {
            model: step.model.clone(),
            effort: step.effort.clone(),
        };
        let resolved = step_profile::resolve(ProfileInput {
            default: &candidate.default,
            ceiling: candidate.ceiling.as_ref(),
            profiled: Some(&wanted),
            failures: 0,
            threshold: 0,
        });
        // A field that cannot be used keeps the step's default
        // (`resolved.rejected`); the other field still counts. An entry that
        // changes nothing is not worth keeping.
        if resolved.setting == candidate.default && resolved.requested_over_ceiling.is_none() {
            continue;
        }
        entries.push(ProfileEntry {
            step_id: step.step_id,
            unit: step.unit,
            setting: resolved.setting,
            rationale: step.rationale,
            requested_over_ceiling: resolved.requested_over_ceiling,
            decided_at: Utc::now(),
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::response::ProfiledStep;

    fn asked() -> ProfileRequest {
        ProfileRequest {
            plan_step: "plan".into(),
            variables: json!({}),
            artifacts: json!({}),
            complexity: None,
            steps: vec![ProfileCandidate {
                step_id: "work".into(),
                default: Setting::new(Some("medium"), Some("medium")),
                ceiling: Some(Setting::new(Some("large"), Some("high"))),
            }],
        }
    }

    fn step(id: &str, model: &str, effort: &str, confident: bool) -> ProfiledStep {
        ProfiledStep {
            step_id: id.into(),
            unit: None,
            model: Some(model.into()),
            effort: Some(effort.into()),
            rationale: "why".into(),
            confident,
        }
    }

    #[test]
    fn a_lower_profile_is_kept() {
        let entries = entries_of(&asked(), vec![step("work", "small", "low", true)]);
        assert_eq!(entries[0].setting, Setting::new(Some("small"), Some("low")));
    }

    #[test]
    fn a_request_above_the_ceiling_starts_on_the_ceiling_and_is_recorded() {
        let entries = entries_of(&asked(), vec![step("work", "large", "max", true)]);
        assert_eq!(
            entries[0].setting,
            Setting::new(Some("large"), Some("high"))
        );
        assert_eq!(
            entries[0].requested_over_ceiling,
            Some(Setting::new(None, Some("max")))
        );
    }

    #[test]
    fn an_unsure_unknown_or_unusable_entry_is_dropped() {
        let entries = entries_of(
            &asked(),
            vec![
                step("work", "small", "low", false),
                step("other", "small", "low", true),
                step("work", "mystery", "mystery", true),
                step("work", "medium", "medium", true),
            ],
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn an_unusable_model_does_not_drop_a_usable_effort() {
        let entries = entries_of(&asked(), vec![step("work", "mystery", "low", true)]);
        assert_eq!(
            entries[0].setting,
            Setting::new(Some("medium"), Some("low"))
        );
    }

    #[test]
    fn a_step_without_an_effort_ceiling_still_takes_a_model() {
        let mut asked = asked();
        asked.steps[0].default = Setting::new(Some("medium"), None);
        asked.steps[0].ceiling = Some(Setting::new(Some("large"), None));
        let entries = entries_of(&asked, vec![step("work", "small", "high", true)]);
        assert_eq!(entries[0].setting, Setting::new(Some("small"), None));
    }
}
