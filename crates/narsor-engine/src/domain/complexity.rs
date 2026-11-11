//! The complexity the plan step reports with its plan.
//!
//! The engine only keeps the assessment and hands it to the conductor, which
//! chooses the profile of the later steps from it. The engine never reads
//! the numbers itself. Every field is optional so that one missing field does
//! not void the rest; a value of the wrong type voids the whole assessment,
//! and the steps then run on their defaults.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The name of the step variable that carries the assessment.
pub const COMPLEXITY_VARIABLE: &str = "complexity";

/// How large and how risky the planned change is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplexityAssessment {
    /// Files the plan changes.
    #[serde(default)]
    pub files: Option<u32>,
    /// Modules the plan touches.
    #[serde(default)]
    pub modules: Option<u32>,
    /// Whether the plan includes a migration or a contract change.
    #[serde(default)]
    pub migration_or_contract: Option<bool>,
    /// Whether the changed code has no tests.
    #[serde(default)]
    pub missing_tests: Option<bool>,
    /// Units of the plan that depend on others, as `"3 -> 1"` style notes.
    #[serde(default)]
    pub unit_dependencies: Vec<String>,
}

impl ComplexityAssessment {
    /// Reads an assessment from a step variable; `None` when the value does
    /// not have the shape.
    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_full_assessment_parses() {
        let value = json!({
            "files": 12, "modules": 3, "migration_or_contract": true,
            "missing_tests": false, "unit_dependencies": ["3 -> 1"],
        });
        let parsed = ComplexityAssessment::from_value(&value).unwrap();
        assert_eq!(parsed.files, Some(12));
        assert_eq!(parsed.migration_or_contract, Some(true));
        assert_eq!(parsed.unit_dependencies, vec!["3 -> 1".to_string()]);
    }

    #[test]
    fn missing_fields_stay_empty() {
        let parsed = ComplexityAssessment::from_value(&json!({ "files": 1 })).unwrap();
        assert_eq!(parsed.modules, None);
        assert!(parsed.unit_dependencies.is_empty());
    }

    #[test]
    fn a_field_of_the_wrong_type_voids_the_assessment() {
        assert_eq!(
            ComplexityAssessment::from_value(&json!({ "files": "many" })),
            None
        );
    }
}
