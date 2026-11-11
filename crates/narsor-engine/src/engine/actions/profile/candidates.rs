//! The steps a `profile` step may profile (R3).

use serde::Serialize;

use crate::domain::WorkflowDefinition;
use crate::engine::step_profile::Setting;

/// One step the profiling agent may give a model and an effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileCandidate {
    /// The step's id.
    pub step_id: String,
    /// What the step is for, as the workflow describes it.
    pub description: Option<String>,
    /// The model and effort the step runs with without a profile: its own,
    /// else the workflow's `globals`.
    pub default: Setting,
    /// The most the step may be raised to. Without one, a value above the
    /// default is lowered to the default.
    pub ceiling: Option<Setting>,
}

/// The steps of `definition` a `profile` step may profile, ordered by id:
/// every `agent` step that is not pinned to its own model and effort
/// ([`crate::domain::ActionConfig::is_profile_candidate`]).
pub fn profile_candidates(definition: &WorkflowDefinition) -> Vec<ProfileCandidate> {
    let mut candidates: Vec<ProfileCandidate> = definition
        .states
        .values()
        .filter_map(|state| {
            let action = state
                .action
                .as_ref()
                .filter(|action| action.is_profile_candidate())?;
            let step = action.harness_step()?;
            Some(ProfileCandidate {
                step_id: state.id.clone(),
                description: state
                    .description
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string),
                default: Setting::default_of(step),
                ceiling: Setting::ceiling_of(step),
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.step_id.cmp(&b.step_id));
    candidates
}
