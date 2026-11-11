//! The model and effort one attempt of an agent step runs with.
//!
//! Three sources feed the profile, in this order: the step's default from the
//! workflow, the entry a `profile` step stored for it, and an escalation
//! after repeated failures. The order, the ceiling rule and the
//! escalation rule live here and nowhere else, as pure functions — the same
//! shape as [`super::retry`].

use serde::{Deserialize, Serialize};

/// Efforts from the cheapest to the strongest, and the model tiers likewise.
/// A workflow value outside them is a validation error. A workflow, a
/// `profile` step and an escalation name a tier; each harness maps a tier to a
/// model name ([`super::actions::harness::HarnessAdapter::model_tiers`]).
pub use crate::config::{EFFORT_SCALE, MODEL_TIERS};

/// Position of a model tier on [`MODEL_TIERS`].
pub fn tier_rank(tier: &str) -> Option<usize> {
    MODEL_TIERS.iter().position(|known| *known == tier)
}

/// Position of an effort on [`EFFORT_SCALE`].
pub fn effort_rank(effort: &str) -> Option<usize> {
    EFFORT_SCALE.iter().position(|known| *known == effort)
}

/// A model and an effort; either may be unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// Model tier, one of [`MODEL_TIERS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort, one of [`EFFORT_SCALE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Setting {
    /// A setting with both fields.
    pub fn new(model: Option<&str>, effort: Option<&str>) -> Self {
        Self {
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
        }
    }

    /// The default a step running a harness names.
    pub fn default_of(config: &crate::domain::HarnessStepConfig) -> Self {
        Self {
            model: config.model.clone(),
            effort: config.effort.clone(),
        }
    }

    /// The ceiling a step running a harness names, if any.
    pub fn ceiling_of(config: &crate::domain::HarnessStepConfig) -> Option<Self> {
        config.ceiling.as_ref().map(|ceiling| Self {
            model: ceiling.model.clone(),
            effort: ceiling.effort.clone(),
        })
    }
}

/// Where the effective profile came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// The workflow default.
    Default,
    /// The entry a `profile` step stored. Read as `conductor` too: the name
    /// before the conductor stopped profiling steps.
    #[serde(alias = "conductor")]
    Profiled,
    /// Raised after repeated failures.
    Escalated,
    /// The profile asked for more than the ceiling; the step starts on the
    /// ceiling.
    Clamped,
}

/// Everything [`resolve`] needs.
#[derive(Debug, Clone, Copy)]
pub struct ProfileInput<'a> {
    /// The step's default from the workflow.
    pub default: &'a Setting,
    /// The ceiling; a missing field falls back to the default's.
    pub ceiling: Option<&'a Setting>,
    /// The stored profile entry for this step.
    pub profiled: Option<&'a Setting>,
    /// Consecutive failed attempts of this step.
    pub failures: u32,
    /// Failures per escalation; `0` turns escalation off.
    pub threshold: u32,
}

/// The result of [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProfile {
    /// What the attempt runs with.
    pub setting: Setting,
    /// Which source decided it.
    pub source: ProfileSource,
    /// What the profile wanted when it was above the ceiling.
    pub requested_over_ceiling: Option<Setting>,
    /// The profile named a model or effort that cannot be used (outside
    /// the tiers or the scale); the default was kept for it.
    pub rejected: bool,
    /// An escalation was due and both fields already stand on the ceiling.
    /// Never set for a step without a ceiling: nothing was reached.
    pub limit_reached: bool,
}

/// Computes the profile of the next attempt.
pub fn resolve(input: ProfileInput<'_>) -> EffectiveProfile {
    let ceiling = Setting {
        model: input
            .ceiling
            .and_then(|c| c.model.clone())
            .or_else(|| input.default.model.clone()),
        effort: input
            .ceiling
            .and_then(|c| c.effort.clone())
            .or_else(|| input.default.effort.clone()),
    };

    let mut setting = input.default.clone();
    let mut source = ProfileSource::Default;
    let mut requested = Setting::default();
    let mut rejected = false;

    if let Some(wanted) = input.profiled {
        if let Some(model) = &wanted.model {
            // The default itself is always allowed.
            if Some(model) != input.default.model.as_ref() {
                match (
                    tier_rank(model),
                    ceiling.model.as_deref().and_then(tier_rank),
                ) {
                    (Some(rank), Some(cap)) if rank <= cap => {
                        setting.model = Some(model.clone());
                        source = ProfileSource::Profiled;
                    }
                    (Some(_), Some(_)) => {
                        setting.model = ceiling.model.clone();
                        requested.model = Some(model.clone());
                        source = ProfileSource::Clamped;
                    }
                    _ => rejected = true,
                }
            }
        }
        if let Some(effort) = &wanted.effort {
            match (
                effort_rank(effort),
                ceiling.effort.as_deref().and_then(effort_rank),
            ) {
                (Some(rank), Some(cap)) if rank <= cap => {
                    if Some(effort) != input.default.effort.as_ref() {
                        source = ProfileSource::Profiled;
                    }
                    setting.effort = Some(effort.clone());
                }
                (Some(_), Some(_)) => {
                    setting.effort = ceiling.effort.clone();
                    requested.effort = Some(effort.clone());
                    source = ProfileSource::Clamped;
                }
                _ => rejected = true,
            }
        }
    }

    let mut limit_reached = false;
    if input.threshold > 0 {
        for _ in 0..input.failures / input.threshold {
            if !raise(&mut setting, &ceiling) {
                // Without a ceiling nothing was reached (D8).
                limit_reached = input.ceiling.is_some();
                break;
            }
            if source != ProfileSource::Clamped {
                source = ProfileSource::Escalated;
            }
        }
    }

    let requested_over_ceiling = (requested != Setting::default()).then_some(requested);
    EffectiveProfile {
        setting,
        source,
        requested_over_ceiling,
        rejected,
        limit_reached,
    }
}

/// Raises the effort one step, or the model one tier when the effort cannot
/// rise; `false` when neither can.
fn raise(setting: &mut Setting, ceiling: &Setting) -> bool {
    if let (Some(current), Some(cap)) = (
        setting.effort.as_deref().and_then(effort_rank),
        ceiling.effort.as_deref().and_then(effort_rank),
    ) {
        if current < cap {
            setting.effort = Some(EFFORT_SCALE[current + 1].to_string());
            return true;
        }
    }
    if let (Some(current), Some(cap)) = (
        setting.model.as_deref().and_then(tier_rank),
        ceiling.model.as_deref().and_then(tier_rank),
    ) {
        if current < cap {
            setting.model = Some(MODEL_TIERS[current + 1].to_string());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        default: &'a Setting,
        ceiling: Option<&'a Setting>,
        profiled: Option<&'a Setting>,
        failures: u32,
    ) -> ProfileInput<'a> {
        ProfileInput {
            default,
            ceiling,
            profiled,
            failures,
            threshold: 3,
        }
    }

    #[test]
    fn without_any_source_the_default_stands() {
        let default = Setting::new(Some("large"), Some("high"));
        let out = resolve(input(&default, None, None, 0));
        assert_eq!(out.setting, default);
        assert_eq!(out.source, ProfileSource::Default);
        assert!(!out.limit_reached && !out.rejected);
    }

    #[test]
    fn an_empty_default_stays_empty() {
        let default = Setting::default();
        let out = resolve(input(&default, None, None, 5));
        assert_eq!(out.setting, default);
    }

    #[test]
    fn a_lower_profile_is_taken() {
        let default = Setting::new(Some("large"), Some("high"));
        let ceiling = default.clone();
        let wanted = Setting::new(Some("small"), Some("low"));
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), 0));
        assert_eq!(out.setting, wanted);
        assert_eq!(out.source, ProfileSource::Profiled);
    }

    #[test]
    fn a_profile_on_the_ceiling_is_taken() {
        let default = Setting::new(Some("medium"), Some("medium"));
        let ceiling = Setting::new(Some("large"), Some("high"));
        let out = resolve(input(&default, Some(&ceiling), Some(&ceiling), 0));
        assert_eq!(out.setting, ceiling);
        assert_eq!(out.source, ProfileSource::Profiled);
        assert_eq!(out.requested_over_ceiling, None);
    }

    #[test]
    fn a_request_above_the_ceiling_starts_on_the_ceiling() {
        let default = Setting::new(Some("medium"), Some("medium"));
        let ceiling = Setting::new(Some("large"), Some("high"));
        let wanted = Setting::new(Some("large"), Some("max"));
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), 0));
        assert_eq!(out.setting, ceiling);
        assert_eq!(out.source, ProfileSource::Clamped);
        assert_eq!(
            out.requested_over_ceiling,
            Some(Setting::new(None, Some("max")))
        );
    }

    #[test]
    fn without_a_ceiling_the_profile_cannot_go_above_the_default() {
        let default = Setting::new(Some("medium"), Some("medium"));
        let wanted = Setting::new(Some("large"), Some("high"));
        let out = resolve(input(&default, None, Some(&wanted), 0));
        assert_eq!(out.setting, default);
        assert!(out.requested_over_ceiling.is_some());
    }

    #[test]
    fn a_model_outside_the_tiers_is_rejected() {
        let default = Setting::new(Some("medium"), Some("medium"));
        let ceiling = Setting::new(Some("large"), Some("high"));
        let wanted = Setting::new(Some("mystery"), None);
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), 0));
        assert_eq!(out.setting, default);
        assert!(out.rejected);
    }

    #[test]
    fn an_effort_outside_the_scale_is_rejected() {
        let default = Setting::new(None, Some("medium"));
        let wanted = Setting::new(None, Some("ultra"));
        let out = resolve(input(&default, None, Some(&wanted), 0));
        assert_eq!(out.setting, default);
        assert!(out.rejected);
    }

    #[test]
    fn the_effort_rises_only_after_the_threshold() {
        let default = Setting::new(Some("medium"), Some("low"));
        let ceiling = Setting::new(Some("large"), Some("high"));
        for failures in 0..3 {
            let out = resolve(input(&default, Some(&ceiling), None, failures));
            assert_eq!(out.setting, default, "failures {failures}");
        }
        let out = resolve(input(&default, Some(&ceiling), None, 3));
        assert_eq!(out.setting, Setting::new(Some("medium"), Some("medium")));
        assert_eq!(out.source, ProfileSource::Escalated);
    }

    #[test]
    fn the_model_rises_when_the_effort_stands_on_the_ceiling() {
        let default = Setting::new(Some("medium"), Some("high"));
        let ceiling = Setting::new(Some("large"), Some("high"));
        let out = resolve(input(&default, Some(&ceiling), None, 3));
        assert_eq!(out.setting, Setting::new(Some("large"), Some("high")));
        assert_eq!(out.source, ProfileSource::Escalated);
    }

    #[test]
    fn both_on_the_ceiling_reports_the_limit() {
        let default = Setting::new(Some("large"), Some("high"));
        let out = resolve(input(&default, Some(&default), None, 3));
        assert_eq!(out.setting, default);
        assert!(out.limit_reached);
    }

    #[test]
    fn without_a_ceiling_nothing_escalates() {
        let default = Setting::new(Some("medium"), Some("medium"));
        let out = resolve(input(&default, None, None, 9));
        assert_eq!(out.setting, default);
        assert!(!out.limit_reached, "no ceiling, so no limit was reached");
    }

    #[test]
    fn an_escalation_starts_from_the_profiled_effort() {
        // AE6: three failures raise the effort the profile chose, not the
        // step's default.
        let default = Setting::new(Some("medium"), Some("low"));
        let ceiling = Setting::new(Some("large"), Some("max"));
        let wanted = Setting::new(None, Some("medium"));
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), 3));
        assert_eq!(out.setting, Setting::new(Some("medium"), Some("high")));
        assert_eq!(out.source, ProfileSource::Escalated);
    }

    #[test]
    fn the_old_name_of_a_profiled_source_still_reads() {
        let source: ProfileSource =
            serde_json::from_value(serde_json::json!("conductor")).expect("the old name");
        assert_eq!(source, ProfileSource::Profiled);
        assert_eq!(
            serde_json::to_value(ProfileSource::Profiled).expect("serialize"),
            serde_json::json!("profiled")
        );
    }

    #[test]
    fn a_zero_threshold_turns_escalation_off() {
        let default = Setting::new(None, Some("low"));
        let ceiling = Setting::new(None, Some("max"));
        let mut i = input(&default, Some(&ceiling), None, 9);
        i.threshold = 0;
        assert_eq!(resolve(i).setting, default);
    }

    #[test]
    fn a_changed_threshold_moves_the_escalation() {
        let default = Setting::new(None, Some("low"));
        let ceiling = Setting::new(None, Some("max"));
        let mut i = input(&default, Some(&ceiling), None, 2);
        i.threshold = 2;
        assert_eq!(resolve(i).setting.effort.as_deref(), Some("medium"));
    }
}
