//! Reading a task's workflow definition back from its launch spec.
//!
//! Lives beside the validator rather than in `crate::domain`, because the
//! snapshot must pass validation and `crate::domain` does not depend on
//! `crate::workflow`.

use serde::Deserialize;

use crate::domain::{LaunchDefinitionError, TaskLaunchSpec, WorkflowDefinition};

impl TaskLaunchSpec {
    /// The workflow definition this task runs, from its snapshot.
    ///
    /// Refuses a spec without one and a snapshot of another workflow than
    /// the spec names: a runner must never fall back to looking the
    /// workflow up by name. Refuses a snapshot this build does not validate
    /// too, so a server and a runner of different builds meet as a reported
    /// problem rather than as a runner that dies before it is ready.
    pub fn definition(&self) -> Result<WorkflowDefinition, LaunchDefinitionError> {
        let value = self
            .definition
            .as_ref()
            .ok_or_else(|| LaunchDefinitionError::Missing(self.workflow_id.clone()))?;
        let definition = WorkflowDefinition::deserialize(value).map_err(|source| {
            LaunchDefinitionError::Unreadable {
                workflow: self.workflow_id.clone(),
                source,
            }
        })?;
        if definition.id != self.workflow_id || definition.version != self.workflow_version {
            return Err(LaunchDefinitionError::Mismatch {
                expected: self.workflow_id.clone(),
                expected_version: self.workflow_version,
                found: definition.id,
                found_version: definition.version,
            });
        }
        definition
            .validate()
            .map_err(|violations| LaunchDefinitionError::Invalid {
                workflow: self.workflow_id.clone(),
                violations: violations.iter().map(ToString::to_string).collect(),
            })?;
        Ok(definition)
    }
}
