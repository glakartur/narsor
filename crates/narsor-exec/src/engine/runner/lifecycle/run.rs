//! Starting an execution and driving it.

use super::*;

impl Engine {
    /// Creates an execution: directories, `state.json`, `context.json`,
    /// `WorkflowStarted` (`docs/dev/domain-model.md` §4.1).
    ///
    /// The definition is validated first: an invalid workflow must leave nothing
    /// on disk. The context starts out holding the inputs, so that a template
    /// may read a starting value through either `{{ inputs.x }}` or
    /// `{{ context.variables.x }}`.
    pub fn start(
        &self,
        definition: &WorkflowDefinition,
        inputs: Value,
        workspace: Workspace,
    ) -> Result<WorkflowExecution, EngineError> {
        definition
            .validate()
            .map_err(|violations| EngineError::InvalidDefinition {
                violations: violations.iter().map(ToString::to_string).collect(),
            })?;

        let mut execution = WorkflowExecution::new(definition, inputs, workspace);

        if let Some(initial) = definition.states.get(&definition.initial_state) {
            if runs_harness(initial) && initial.max_entries.is_some() {
                count_entry(&mut execution, &initial.id)?;
            }
        }
        step_memory::count_step_entry(&mut execution, &definition.initial_state)?;

        if let Some(object) = execution.inputs.as_object() {
            execution.context.update_variables(object.clone());
        }
        execution.context.workspace = Some(WorkspaceInfo {
            id: execution.workspace.id.clone(),
            path: execution.workspace.path.clone(),
            workspace_type: format!("{:?}", execution.workspace.workspace_type).to_lowercase(),
        });

        self.store.create_execution_dirs(&execution.id)?;
        self.store.save_execution(&execution)?;

        self.emit(
            &execution.id,
            EventData::WorkflowStarted {
                workflow_id: definition.id.clone(),
                workflow_version: definition.version,
                inputs: execution.inputs.clone(),
            },
        )?;

        Ok(execution)
    }

    /// Starts an execution and runs it until it stops.
    pub async fn start_and_run(
        &self,
        definition: &WorkflowDefinition,
        inputs: Value,
        workspace: Workspace,
    ) -> Result<WorkflowExecution, EngineError> {
        let execution = self.start(definition, inputs, workspace)?;
        self.run_loop(definition, execution).await
    }

    /// Runs a stored execution until it reaches a terminal state, suspends or
    /// is paused.
    ///
    /// Only a `Running` execution can be run; a paused one goes through
    /// [`Engine::resume`] or [`Engine::deliver_human_input`], which is what
    /// keeps an answer from being delivered twice.
    pub async fn run(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
    ) -> Result<WorkflowExecution, EngineError> {
        self.observed(execution_id, async {
            let _guard = self.begin_run(execution_id)?;
            let execution = self.store.load_execution(execution_id)?;
            if execution.status != ExecutionStatus::Running {
                return Err(self.not_runnable(&execution, "run"));
            }
            self.run_loop(definition, execution).await
        })
        .await
    }

    /// Runs a stored execution like [`Engine::run`], but stops after `steps`
    /// step boundaries — each one a transition taken and persisted — even if
    /// the workflow could go on.
    ///
    /// The execution is left `Running` at a clean boundary, exactly as a
    /// process that died right after that step would leave it, which is what
    /// makes it the seam for proving that a takeover resumes correctly from any
    /// point.
    pub async fn run_steps(
        &self,
        definition: &WorkflowDefinition,
        execution_id: &str,
        steps: usize,
    ) -> Result<WorkflowExecution, EngineError> {
        let execution = self.store.load_execution(execution_id)?;
        if execution.status != ExecutionStatus::Running {
            return Err(self.not_runnable(&execution, "run"));
        }
        self.step_loop(definition, execution, Some(steps)).await
    }
}
