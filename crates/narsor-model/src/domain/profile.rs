//! The model and effort pair a step profile names.

use serde::{Deserialize, Serialize};

#[cfg(doc)]
use crate::config::{EFFORT_SCALE, MODEL_TIERS};

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
    pub fn default_of(config: &super::HarnessStepConfig) -> Self {
        Self {
            model: config.model.clone(),
            effort: config.effort.clone(),
        }
    }

    /// The ceiling a step running a harness names, if any.
    pub fn ceiling_of(config: &super::HarnessStepConfig) -> Option<Self> {
        config.ceiling.as_ref().map(|ceiling| Self {
            model: ceiling.model.clone(),
            effort: ceiling.effort.clone(),
        })
    }
}
