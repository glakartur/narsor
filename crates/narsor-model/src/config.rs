//! Shared process configuration, plus the rank of model tiers and efforts.

pub use narsor_shared::config::*;

/// Position of a model tier on [`MODEL_TIERS`].
pub fn tier_rank(tier: &str) -> Option<usize> {
    MODEL_TIERS.iter().position(|known| *known == tier)
}

/// Position of an effort on [`EFFORT_SCALE`].
pub fn effort_rank(effort: &str) -> Option<usize> {
    EFFORT_SCALE.iter().position(|known| *known == effort)
}
