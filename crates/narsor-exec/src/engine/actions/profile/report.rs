//! What a `profile` step tells the operator (R7, R11): one Narsor entry in
//! the attempt's conversation, and a one-line summary that becomes the
//! step's reason.

use narsor_protocol::chat::{ChatElement, TechnicalCategory};
use serde_json::{json, Value};

use super::entries::{CheckedProfile, EntryOutcome};
use crate::engine::actions::session::ConversationSink;

/// The step's reason: how many entries were stored and how many skipped.
pub fn summary(profile: &CheckedProfile) -> String {
    format!(
        "Step profile: {} stored, {} skipped",
        profile.stored.len(),
        profile.skipped()
    )
}

/// The conversation entry of a checked profile: a line per entry with what
/// was asked, what was stored, what was clamped and why. Every text the
/// agent wrote is already sanitized and bounded
/// ([`super::entries::check_profile`]).
pub fn report_text(profile: &CheckedProfile) -> String {
    let mut text = format!("{}.", summary(profile));
    for outcome in &profile.outcomes {
        text.push_str("\n- ");
        text.push_str(&outcome_line(outcome));
    }
    if profile.dropped > 0 {
        text.push_str(&format!(
            "\n- {} more entries were not read: an answer is read up to its first {}.",
            profile.dropped,
            super::answer::MAX_PROFILE_ENTRIES
        ));
    }
    text
}

/// One entry: `step: stored model …, effort … (asked model …, effort …);
/// notes. Rationale: …`.
fn outcome_line(outcome: &EntryOutcome) -> String {
    let step = if outcome.step.is_empty() {
        "(no step)"
    } else {
        outcome.step.as_str()
    };
    let mut line = format!("`{step}`: ");
    match &outcome.stored {
        Some(entry) => line.push_str(&format!(
            "stored model {}, effort {}",
            value_or_default(entry.model.as_deref()),
            value_or_default(entry.effort.as_deref())
        )),
        None => line.push_str("not stored"),
    }
    line.push_str(&format!(
        " (asked model {}, effort {})",
        value_or_default(outcome.requested.model.as_deref()),
        value_or_default(outcome.requested.effort.as_deref())
    ));
    for note in &outcome.notes {
        line.push_str("; ");
        line.push_str(note);
    }
    if !outcome.rationale.is_empty() {
        line.push_str(". Rationale: ");
        line.push_str(&outcome.rationale);
    }
    line
}

fn value_or_default(value: Option<&str>) -> String {
    value.map_or("default".to_string(), |value| format!("`{value}`"))
}

/// The structured details of the entry, for a view that expands it.
fn report_data(profile: &CheckedProfile) -> Value {
    json!({
        "event": "step_profile",
        "stored": profile.stored.len(),
        "skipped": profile.skipped(),
        "steps": profile.outcomes.iter().map(|outcome| json!({
            "step": outcome.step,
            "requested": outcome.requested,
            "stored": outcome.stored.as_ref().map(|entry| json!({
                "model": entry.model,
                "effort": entry.effort,
            })),
            "clamped": outcome.clamped,
            "notes": outcome.notes,
            "rationale": outcome.rationale,
        })).collect::<Vec<_>>(),
    })
}

/// Appends the profile's entry to the attempt's conversation: a Narsor
/// entry whose text is [`report_text`] and whose one element is a
/// [`TechnicalCategory::Process`] event, summarized by [`summary`].
pub(crate) fn record_profile(conversation: &ConversationSink, profile: &CheckedProfile) {
    let element = ChatElement::Technical {
        category: TechnicalCategory::Process,
        summary: summary(profile),
        data: Some(report_data(profile)),
    };
    conversation.append_narsor(&report_text(profile), &[element]);
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::super::answer::{ProfileAnswer, ProposedEntry, ProposedSetting};
    use super::super::candidates::ProfileCandidate;
    use super::super::entries::check_profile;
    use super::*;
    use crate::engine::step_profile::Setting;

    fn profile(entries: Vec<ProposedEntry>) -> CheckedProfile {
        check_profile(
            &[ProfileCandidate {
                step_id: "work".into(),
                description: None,
                default: Setting::new(Some("medium"), Some("low")),
                ceiling: Some(Setting::new(Some("large"), Some("high"))),
            }],
            &ProfileAnswer {
                entries,
                dropped: 0,
                reason: String::new(),
            },
            "profile",
            Utc::now(),
        )
    }

    fn proposed(step: &str, effort: &str, rationale: &str) -> ProposedEntry {
        ProposedEntry::Proposed(ProposedSetting {
            step: step.into(),
            model: None,
            effort: Some(effort.into()),
            rationale: rationale.into(),
            confident: true,
        })
    }

    /// AE4: the request, the clamp and the stored value are all shown.
    #[test]
    fn a_clamped_entry_shows_the_request_and_the_clamp() {
        let text = report_text(&profile(vec![proposed("work", "max", "big change")]));
        assert!(
            text.starts_with("Step profile: 1 stored, 0 skipped."),
            "{text}"
        );
        assert!(
            text.contains(
                "`work`: stored model default, effort `high` (asked model default, effort \
                 `max`); effort `max` is above the ceiling: clamped to `high`. Rationale: big \
                 change"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_skipped_entry_says_why() {
        let text = report_text(&profile(vec![proposed("nowhere", "high", "")]));
        assert!(
            text.contains("Step profile: 0 stored, 1 skipped."),
            "{text}"
        );
        assert!(text.contains("`nowhere`: not stored"), "{text}");
        assert!(text.contains("not a step this profile may set"), "{text}");
    }

    #[test]
    fn control_characters_of_the_rationale_never_reach_the_entry() {
        let checked = profile(vec![proposed("work", "high", "fine\u{1b}[2J\u{7} indeed")]);
        let text = report_text(&checked);
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text:?}"
        );
        assert!(text.contains("fine[2J indeed"), "{text:?}");
        let data = report_data(&checked).to_string();
        assert!(!data.contains("\\u001b"), "{data}");
    }
}
