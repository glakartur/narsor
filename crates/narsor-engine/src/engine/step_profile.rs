//! The model and effort one attempt of an agent step runs with.
//!
//! Three sources feed the profile, in this order: the step's default from the
//! workflow, a profile the conductor stored after the plan step, and an
//! escalation after repeated failures. The order, the ceiling rule and the
//! escalation rule live here and nowhere else, as pure functions — the same
//! shape as [`super::retry`].

use serde::{Deserialize, Serialize};

/// Efforts from the cheapest to the strongest. A workflow value outside this
/// scale is a validation error.
pub const EFFORT_SCALE: [&str; 4] = ["low", "medium", "high", "max"];

/// Position of an effort on [`EFFORT_SCALE`].
pub fn effort_rank(effort: &str) -> Option<usize> {
    EFFORT_SCALE.iter().position(|known| *known == effort)
}

/// A model and an effort; either may be unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setting {
    /// Model name, as the harness takes it.
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

    /// The default an agent step names.
    pub fn default_of(config: &crate::domain::AgentActionConfig) -> Self {
        Self {
            model: config.model.clone(),
            effort: config.effort.clone(),
        }
    }

    /// The ceiling an agent step names, if any.
    pub fn ceiling_of(config: &crate::domain::AgentActionConfig) -> Option<Self> {
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
    /// The profile the conductor stored.
    Conductor,
    /// Raised after repeated failures.
    Escalated,
    /// The conductor asked for more than the ceiling; the step starts on the
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
    /// The conductor's profile for this step.
    pub conductor: Option<&'a Setting>,
    /// Models from the cheapest. Models outside it are never compared.
    pub ladder: &'a [String],
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
    /// What the conductor wanted when it was above the ceiling.
    pub requested_over_ceiling: Option<Setting>,
    /// The conductor named a model or effort that cannot be used (outside the
    /// scale or the ladder); the default was kept for it.
    pub rejected: bool,
    /// An escalation was due and both fields already stand on the ceiling.
    pub limit_reached: bool,
}

fn model_rank(ladder: &[String], model: &str) -> Option<usize> {
    ladder.iter().position(|known| known == model)
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

    if let Some(wanted) = input.conductor {
        if let Some(model) = &wanted.model {
            // The default itself is always allowed.
            if Some(model) != input.default.model.as_ref() {
                match (
                    model_rank(input.ladder, model),
                    ceiling
                        .model
                        .as_deref()
                        .and_then(|c| model_rank(input.ladder, c)),
                ) {
                    (Some(rank), Some(cap)) if rank <= cap => {
                        setting.model = Some(model.clone());
                        source = ProfileSource::Conductor;
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
                        source = ProfileSource::Conductor;
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
            if !raise(&mut setting, &ceiling, input.ladder) {
                limit_reached = true;
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

/// Raises the effort one step, or the model one rung when the effort cannot
/// rise; `false` when neither can.
fn raise(setting: &mut Setting, ceiling: &Setting, ladder: &[String]) -> bool {
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
        setting.model.as_deref().and_then(|m| model_rank(ladder, m)),
        ceiling.model.as_deref().and_then(|m| model_rank(ladder, m)),
    ) {
        if current < cap {
            setting.model = Some(ladder[current + 1].clone());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> Vec<String> {
        ["haiku", "sonnet", "opus"].map(String::from).to_vec()
    }

    fn input<'a>(
        default: &'a Setting,
        ceiling: Option<&'a Setting>,
        conductor: Option<&'a Setting>,
        ladder: &'a [String],
        failures: u32,
    ) -> ProfileInput<'a> {
        ProfileInput {
            default,
            ceiling,
            conductor,
            ladder,
            failures,
            threshold: 3,
        }
    }

    #[test]
    fn without_any_source_the_default_stands() {
        let default = Setting::new(Some("opus"), Some("high"));
        let out = resolve(input(&default, None, None, &[], 0));
        assert_eq!(out.setting, default);
        assert_eq!(out.source, ProfileSource::Default);
        assert!(!out.limit_reached && !out.rejected);
    }

    #[test]
    fn an_empty_default_stays_empty() {
        let default = Setting::default();
        let out = resolve(input(&default, None, None, &[], 5));
        assert_eq!(out.setting, default);
    }

    #[test]
    fn a_lower_conductor_profile_is_taken() {
        let l = ladder();
        let default = Setting::new(Some("opus"), Some("high"));
        let ceiling = default.clone();
        let wanted = Setting::new(Some("haiku"), Some("low"));
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), &l, 0));
        assert_eq!(out.setting, wanted);
        assert_eq!(out.source, ProfileSource::Conductor);
    }

    #[test]
    fn a_profile_on_the_ceiling_is_taken() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("medium"));
        let ceiling = Setting::new(Some("opus"), Some("high"));
        let out = resolve(input(&default, Some(&ceiling), Some(&ceiling), &l, 0));
        assert_eq!(out.setting, ceiling);
        assert_eq!(out.source, ProfileSource::Conductor);
        assert_eq!(out.requested_over_ceiling, None);
    }

    #[test]
    fn a_request_above_the_ceiling_starts_on_the_ceiling() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("medium"));
        let ceiling = Setting::new(Some("opus"), Some("high"));
        let wanted = Setting::new(Some("opus"), Some("max"));
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), &l, 0));
        assert_eq!(out.setting, ceiling);
        assert_eq!(out.source, ProfileSource::Clamped);
        assert_eq!(
            out.requested_over_ceiling,
            Some(Setting::new(None, Some("max")))
        );
    }

    #[test]
    fn without_a_ceiling_the_conductor_cannot_go_above_the_default() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("medium"));
        let wanted = Setting::new(Some("opus"), Some("high"));
        let out = resolve(input(&default, None, Some(&wanted), &l, 0));
        assert_eq!(out.setting, default);
        assert!(out.requested_over_ceiling.is_some());
    }

    #[test]
    fn a_model_outside_the_ladder_is_rejected() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("medium"));
        let ceiling = Setting::new(Some("opus"), Some("high"));
        let wanted = Setting::new(Some("mystery"), None);
        let out = resolve(input(&default, Some(&ceiling), Some(&wanted), &l, 0));
        assert_eq!(out.setting, default);
        assert!(out.rejected);
    }

    #[test]
    fn an_effort_outside_the_scale_is_rejected() {
        let default = Setting::new(None, Some("medium"));
        let wanted = Setting::new(None, Some("ultra"));
        let out = resolve(input(&default, None, Some(&wanted), &[], 0));
        assert_eq!(out.setting, default);
        assert!(out.rejected);
    }

    #[test]
    fn the_effort_rises_only_after_the_threshold() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("low"));
        let ceiling = Setting::new(Some("opus"), Some("high"));
        for failures in 0..3 {
            let out = resolve(input(&default, Some(&ceiling), None, &l, failures));
            assert_eq!(out.setting, default, "failures {failures}");
        }
        let out = resolve(input(&default, Some(&ceiling), None, &l, 3));
        assert_eq!(out.setting, Setting::new(Some("sonnet"), Some("medium")));
        assert_eq!(out.source, ProfileSource::Escalated);
    }

    #[test]
    fn the_model_rises_when_the_effort_stands_on_the_ceiling() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("high"));
        let ceiling = Setting::new(Some("opus"), Some("high"));
        let out = resolve(input(&default, Some(&ceiling), None, &l, 3));
        assert_eq!(out.setting, Setting::new(Some("opus"), Some("high")));
        assert_eq!(out.source, ProfileSource::Escalated);
    }

    #[test]
    fn both_on_the_ceiling_reports_the_limit() {
        let l = ladder();
        let default = Setting::new(Some("opus"), Some("high"));
        let out = resolve(input(&default, Some(&default), None, &l, 3));
        assert_eq!(out.setting, default);
        assert!(out.limit_reached);
    }

    #[test]
    fn without_a_ceiling_nothing_escalates() {
        let l = ladder();
        let default = Setting::new(Some("sonnet"), Some("medium"));
        let out = resolve(input(&default, None, None, &l, 9));
        assert_eq!(out.setting, default);
        assert!(out.limit_reached);
    }

    #[test]
    fn a_zero_threshold_turns_escalation_off() {
        let default = Setting::new(None, Some("low"));
        let ceiling = Setting::new(None, Some("max"));
        let mut i = input(&default, Some(&ceiling), None, &[], 9);
        i.threshold = 0;
        assert_eq!(resolve(i).setting, default);
    }

    #[test]
    fn a_changed_threshold_moves_the_escalation() {
        let default = Setting::new(None, Some("low"));
        let ceiling = Setting::new(None, Some("max"));
        let mut i = input(&default, Some(&ceiling), None, &[], 2);
        i.threshold = 2;
        assert_eq!(resolve(i).setting.effort.as_deref(), Some("medium"));
    }
}
