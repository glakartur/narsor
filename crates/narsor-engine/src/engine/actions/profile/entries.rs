//! Checking the profiling agent's entries against the steps it may profile
//! (R4, R6, R7) and turning what holds up into
//! [`StepProfileEntry`] values.
//!
//! Each entry goes through the same steps, in this order:
//!
//! 1. An entry that cannot be read, or names a step that is not on the list
//!    (unknown, pinned, not an `agent` step), is skipped with a note.
//! 2. A step gets one entry: the **first** entry for a step counts, a later
//!    one is skipped with a note.
//! 3. `confident: false` is skipped (R4).
//! 4. The model and the effort are judged separately through
//!    [`step_profile::resolve`], the one rule: a value outside the tiers or
//!    the scale keeps the step's default, and the other value still counts
//!    (R6); a value above the ceiling is clamped to it, and the request is
//!    kept (R7); without a ceiling, a value above the default is clamped to
//!    the default.
//! 5. An entry that changes nothing and clamped nothing is skipped (R6).

use chrono::{DateTime, Utc};
use narsor_protocol::display::{without_unsafe_chars, without_unsafe_chars_multiline};

use super::answer::{ProfileAnswer, ProposedEntry, ProposedSetting};
use super::candidates::ProfileCandidate;
use crate::engine::step_profile::{self, effort_rank, tier_rank, ProfileInput, Setting};
use crate::persistence::StepProfileEntry;

/// The longest rationale kept, in characters.
pub const MAX_RATIONALE_CHARS: usize = 1_000;

/// The longest step id, model or effort a note quotes, in characters: the
/// agent's text is untrusted.
const MAX_QUOTED_CHARS: usize = 80;

/// The checked profile: what is stored, and what happened to every entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedProfile {
    /// The entries to store, one per step, in the order the agent gave them.
    pub stored: Vec<StepProfileEntry>,
    /// What happened to each entry the agent gave, in its order.
    pub outcomes: Vec<EntryOutcome>,
    /// How many entries were dropped unread past the limit.
    pub dropped: usize,
}

impl CheckedProfile {
    /// How many entries were skipped, the dropped ones included.
    pub fn skipped(&self) -> usize {
        self.outcomes.len() - self.stored.len() + self.dropped
    }
}

/// What happened to one entry of the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryOutcome {
    /// The step the entry named, sanitized and bounded; empty when it named
    /// none.
    pub step: String,
    /// The model and effort asked for, sanitized and bounded.
    pub requested: Setting,
    /// The stored entry, when the entry holds up.
    pub stored: Option<StepProfileEntry>,
    /// Why the entry was skipped, or what was changed in it.
    pub notes: Vec<String>,
    /// Whether a value was clamped to the ceiling (or the default).
    pub clamped: bool,
    /// The rationale, sanitized and bounded.
    pub rationale: String,
}

/// Checks `answer` against `candidates`. `profiled_by` is the `profile`
/// step, `decided_at` the time the profile was decided.
pub fn check_profile(
    candidates: &[ProfileCandidate],
    answer: &ProfileAnswer,
    profiled_by: &str,
    decided_at: DateTime<Utc>,
) -> CheckedProfile {
    let mut stored: Vec<StepProfileEntry> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut outcomes = Vec::new();
    for entry in &answer.entries {
        let outcome = match entry {
            ProposedEntry::Malformed { problem } => {
                skipped(&ProposedSetting::default(), format!("skipped: {problem}"))
            }
            ProposedEntry::Proposed(proposed) => {
                match candidates.iter().find(|c| c.step_id == proposed.step) {
                    None => skipped(
                        proposed,
                        "skipped: not a step this profile may set (unknown, pinned to its own \
                         model and effort, or not an agent step)"
                            .to_string(),
                    ),
                    Some(candidate) if seen.contains(&candidate.step_id.as_str()) => skipped(
                        proposed,
                        "skipped: the step already has an entry, and the first one counts"
                            .to_string(),
                    ),
                    Some(candidate) => {
                        seen.push(&candidate.step_id);
                        let outcome = if proposed.confident {
                            checked_entry(candidate, proposed, profiled_by, decided_at)
                        } else {
                            skipped(proposed, "skipped: the agent is not confident".to_string())
                        };
                        if let Some(entry) = &outcome.stored {
                            stored.push(entry.clone());
                        }
                        outcome
                    }
                }
            }
        };
        outcomes.push(outcome);
    }
    CheckedProfile {
        stored,
        outcomes,
        dropped: answer.dropped,
    }
}

/// An entry for a step on the list, from a confident agent.
fn checked_entry(
    candidate: &ProfileCandidate,
    proposed: &ProposedSetting,
    profiled_by: &str,
    decided_at: DateTime<Utc>,
) -> EntryOutcome {
    let wanted = Setting::new(proposed.model.as_deref(), proposed.effort.as_deref());
    let resolved = step_profile::resolve(ProfileInput {
        default: &candidate.default,
        ceiling: candidate.ceiling.as_ref(),
        profiled: Some(&wanted),
        failures: 0,
        threshold: 0,
    });
    let over = resolved.requested_over_ceiling.clone().unwrap_or_default();
    let limit = if candidate.ceiling.is_some() {
        "the ceiling"
    } else {
        "the default (the step has no ceiling)"
    };

    let mut outcome = outcome_of(proposed);
    for (field, asked, default, kept, clamped_from, known) in [
        (
            "model",
            &proposed.model,
            &candidate.default.model,
            &resolved.setting.model,
            &over.model,
            proposed.model.as_deref().and_then(tier_rank).is_some(),
        ),
        (
            "effort",
            &proposed.effort,
            &candidate.default.effort,
            &resolved.setting.effort,
            &over.effort,
            proposed.effort.as_deref().and_then(effort_rank).is_some(),
        ),
    ] {
        let Some(asked) = asked else { continue };
        if clamped_from.is_some() {
            outcome.clamped = true;
            outcome.notes.push(format!(
                "{field} `{}` is above {limit}: clamped to `{}`",
                quoted(asked),
                kept.as_deref().unwrap_or("-")
            ));
        } else if kept != &Some(asked.clone()) && Some(asked) != default.as_ref() {
            outcome.notes.push(if known {
                format!(
                    "{field} `{}` cannot be compared with the step's settings: the default stays",
                    quoted(asked)
                )
            } else {
                format!(
                    "{field} `{}` is not a known {}: the default stays",
                    quoted(asked),
                    if field == "model" {
                        "model tier"
                    } else {
                        "effort"
                    }
                )
            });
        }
    }

    let model = resolved
        .setting
        .model
        .clone()
        .filter(|model| Some(model) != candidate.default.model.as_ref());
    let effort = resolved
        .setting
        .effort
        .clone()
        .filter(|effort| Some(effort) != candidate.default.effort.as_ref());
    if model.is_none() && effort.is_none() && !outcome.clamped {
        outcome
            .notes
            .push("skipped: the same as the step's defaults".to_string());
        return outcome;
    }

    outcome.stored = Some(StepProfileEntry {
        step_id: candidate.step_id.clone(),
        model,
        effort,
        rationale: outcome.rationale.clone(),
        requested_model: outcome.requested.model.clone(),
        requested_effort: outcome.requested.effort.clone(),
        profiled_by: profiled_by.to_string(),
        decided_at,
    });
    outcome
}

/// The outcome of an entry that is not stored, with `note`.
fn skipped(proposed: &ProposedSetting, note: String) -> EntryOutcome {
    let mut outcome = outcome_of(proposed);
    outcome.notes.push(note);
    outcome
}

/// The outcome of `proposed` before it is judged: nothing stored, no note.
fn outcome_of(proposed: &ProposedSetting) -> EntryOutcome {
    EntryOutcome {
        step: quoted(&proposed.step),
        requested: Setting {
            model: proposed.model.as_deref().map(quoted),
            effort: proposed.effort.as_deref().map(quoted),
        },
        stored: None,
        notes: Vec::new(),
        clamped: false,
        rationale: bounded(
            &without_unsafe_chars_multiline(&proposed.rationale),
            MAX_RATIONALE_CHARS,
        ),
    }
}

/// Untrusted text quoted on one line: sanitized and bounded.
fn quoted(text: &str) -> String {
    bounded(&without_unsafe_chars(text), MAX_QUOTED_CHARS)
}

/// `text` cut to `max` characters, marked when cut.
fn bounded(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        id: &str,
        default: (&str, &str),
        ceiling: Option<(&str, &str)>,
    ) -> ProfileCandidate {
        ProfileCandidate {
            step_id: id.into(),
            description: None,
            default: Setting::new(Some(default.0), Some(default.1)),
            ceiling: ceiling.map(|(model, effort)| Setting::new(Some(model), Some(effort))),
        }
    }

    /// `work`: medium/low, ceiling large/high. `review`: medium/medium, no
    /// ceiling.
    fn candidates() -> Vec<ProfileCandidate> {
        vec![
            candidate("review", ("medium", "medium"), None),
            candidate("work", ("medium", "low"), Some(("large", "high"))),
        ]
    }

    fn entry(step: &str, model: &str, effort: &str, confident: bool) -> ProposedEntry {
        ProposedEntry::Proposed(ProposedSetting {
            step: step.into(),
            model: Some(model.into()),
            effort: Some(effort.into()),
            rationale: "why".into(),
            confident,
        })
    }

    fn check(entries: Vec<ProposedEntry>) -> CheckedProfile {
        check_profile(
            &candidates(),
            &ProfileAnswer {
                entries,
                dropped: 0,
                reason: String::new(),
            },
            "profile",
            Utc::now(),
        )
    }

    fn stored(profile: &CheckedProfile, step: &str) -> Option<StepProfileEntry> {
        profile
            .stored
            .iter()
            .find(|entry| entry.step_id == step)
            .cloned()
    }

    #[test]
    fn a_step_within_its_ceiling_takes_both_values() {
        let profile = check(vec![entry("work", "large", "high", true)]);
        let work = stored(&profile, "work").expect("stored");
        assert_eq!(work.model.as_deref(), Some("large"));
        assert_eq!(work.effort.as_deref(), Some("high"));
        assert_eq!(work.requested_model.as_deref(), Some("large"));
        assert_eq!(work.profiled_by, "profile");
        assert_eq!(work.rationale, "why");
    }

    /// AE3: an effort outside the scale keeps the default; the model counts.
    #[test]
    fn an_unknown_effort_keeps_the_default_and_the_model_still_counts() {
        let profile = check(vec![entry("work", "large", "extreme", true)]);
        let work = stored(&profile, "work").expect("stored");
        assert_eq!(work.model.as_deref(), Some("large"));
        assert_eq!(work.effort, None, "None keeps the step's default");
        assert_eq!(work.setting(), Setting::new(Some("large"), None));
        let notes = profile.outcomes[0].notes.join("\n");
        assert!(
            notes.contains("effort `extreme` is not a known effort"),
            "{notes}"
        );
    }

    /// AE4: above the ceiling, clamped, and the request kept.
    #[test]
    fn an_effort_above_the_ceiling_is_clamped_and_the_request_kept() {
        let profile = check(vec![entry("work", "medium", "max", true)]);
        let work = stored(&profile, "work").expect("stored");
        assert_eq!(work.effort.as_deref(), Some("high"));
        assert_eq!(work.requested_effort.as_deref(), Some("max"));
        assert_eq!(work.model, None, "the default model is no change");
        assert!(profile.outcomes[0].clamped);
        let notes = profile.outcomes[0].notes.join("\n");
        assert!(
            notes.contains("effort `max` is above the ceiling: clamped to `high`"),
            "{notes}"
        );
    }

    #[test]
    fn without_a_ceiling_a_value_above_the_default_is_lowered_to_it() {
        let profile = check(vec![entry("review", "large", "low", true)]);
        let review = stored(&profile, "review").expect("stored: the effort is lower");
        assert_eq!(review.model, None);
        assert_eq!(review.requested_model.as_deref(), Some("large"));
        assert_eq!(review.effort.as_deref(), Some("low"));
        let notes = profile.outcomes[0].notes.join("\n");
        assert!(notes.contains("clamped to `medium`"), "{notes}");
        assert!(notes.contains("the step has no ceiling"), "{notes}");
    }

    /// AE8.
    #[test]
    fn an_unsure_entry_is_not_stored() {
        let profile = check(vec![entry("work", "large", "high", false)]);
        assert!(profile.stored.is_empty());
        assert_eq!(
            profile.outcomes[0].notes,
            ["skipped: the agent is not confident"]
        );
    }

    #[test]
    fn an_entry_equal_to_the_defaults_is_not_stored() {
        let profile = check(vec![entry("work", "medium", "low", true)]);
        assert!(profile.stored.is_empty());
        assert_eq!(
            profile.outcomes[0].notes,
            ["skipped: the same as the step's defaults"]
        );
    }

    #[test]
    fn an_unknown_or_pinned_step_is_skipped_with_a_note() {
        let profile = check(vec![
            entry("build", "large", "high", true),
            entry("nowhere", "large", "high", true),
        ]);
        assert!(profile.stored.is_empty());
        for outcome in &profile.outcomes {
            assert!(
                outcome.notes[0].starts_with("skipped: not a step this profile may set"),
                "{:?}",
                outcome.notes
            );
        }
        assert_eq!(profile.outcomes[0].step, "build");
    }

    /// The rule for two entries of one step: the first counts, even when it
    /// is not stored.
    #[test]
    fn the_first_entry_for_a_step_counts() {
        let profile = check(vec![
            entry("work", "large", "high", true),
            entry("work", "small", "low", true),
        ]);
        assert_eq!(profile.stored.len(), 1);
        assert_eq!(
            stored(&profile, "work").and_then(|e| e.model).as_deref(),
            Some("large")
        );
        assert_eq!(
            profile.outcomes[1].notes,
            ["skipped: the step already has an entry, and the first one counts"]
        );

        let unsure_first = check(vec![
            entry("work", "large", "high", false),
            entry("work", "small", "low", true),
        ]);
        assert!(unsure_first.stored.is_empty());
    }

    #[test]
    fn a_malformed_entry_is_skipped_and_the_others_still_count() {
        let profile = check(vec![
            ProposedEntry::Malformed {
                problem: "the entry names no step",
            },
            entry("work", "large", "high", true),
        ]);
        assert_eq!(profile.stored.len(), 1);
        assert_eq!(profile.skipped(), 1);
        assert_eq!(
            profile.outcomes[0].notes,
            ["skipped: the entry names no step"]
        );
    }

    #[test]
    fn an_empty_answer_stores_nothing() {
        let profile = check(Vec::new());
        assert!(profile.stored.is_empty() && profile.outcomes.is_empty());
    }

    #[test]
    fn untrusted_text_is_sanitized_and_bounded() {
        let profile = check(vec![ProposedEntry::Proposed(ProposedSetting {
            step: format!("work\u{1b}[31m{}", "x".repeat(500)),
            model: Some("large\u{7}".into()),
            effort: None,
            rationale: format!("line one\u{1b}[2J\nline two{}", "y".repeat(2_000)),
            confident: true,
        })]);
        let outcome = &profile.outcomes[0];
        assert!(!outcome.step.contains('\u{1b}'));
        assert!(outcome.step.chars().count() <= MAX_QUOTED_CHARS + 1);
        assert!(!outcome.rationale.contains('\u{1b}'));
        assert!(outcome.rationale.contains("line one"));
        assert!(outcome.rationale.contains('\n'));
        assert!(outcome.rationale.chars().count() <= MAX_RATIONALE_CHARS + 1);
        assert_eq!(outcome.requested.model.as_deref(), Some("large"));
    }
}
