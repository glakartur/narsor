//! The checkpoint written after each step.

use super::*;

impl Engine {
    /// Captures the workspace at a step boundary, if checkpointing is on.
    ///
    /// A failing checkpoint does not fail the step: the execution's own record
    /// is already durable, and refusing to go on because a *copy* could not be
    /// made would turn a recoverable situation into a stopped workflow. The
    /// failure is logged, and the previous checkpoint is left intact (R25).
    ///
    /// `preserve` comes from the workflow's own repository declaration
    /// (`preserve_paths` of each repository) — the definition, not the
    /// provisioned `Workspace` record, because the latter does not carry it
    /// (it only exists to describe *where* the workspace lives, not how it was
    /// declared).
    pub(crate) fn write_checkpoint(
        &self,
        definition: &WorkflowDefinition,
        execution: &WorkflowExecution,
    ) {
        if !self.checkpoints {
            return;
        }

        // One bundle per repository (KTD10): each has its own base commit and
        // its own `preserve_paths`, and one failing does not stop the others.
        let paths = self.store.paths();
        for repository in execution.workspace.repository_entries() {
            let base = match &repository.base_commit {
                Some(commit) => crate::workspace::Base::Commit(commit.clone()),
                None => crate::workspace::Base::None,
            };
            let preserve: &[String] = definition
                .repositories
                .iter()
                .find(|declared| declared.name == repository.name)
                .or_else(|| {
                    definition
                        .primary_repository()
                        .filter(|_| repository.primary)
                })
                .map_or(&[], |declared| &declared.preserve_paths);
            let scratch = paths.checkpoint_scratch(&execution.id, &repository.name);
            let bundle = paths.checkpoint_file(&execution.id, &repository.name);
            let request = crate::workspace::CaptureRequest {
                work_tree: std::path::Path::new(&repository.path),
                base,
                scratch: &scratch,
                bundle: &bundle,
                preserve,
            };
            if let Err(error) = crate::workspace::capture(request) {
                tracing::warn!(
                    execution_id = %execution.id,
                    repository = %repository.name,
                    %error,
                    "could not checkpoint the workspace; the previous checkpoint is kept"
                );
            }
        }
    }
}
