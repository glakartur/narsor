//! The preparation of an attempt that runs on a harness.
//!
//! Before such an action reaches its executor, the engine adds the step's
//! history, pins the default harness, applies the step profile, and hands
//! over a pending answer of the operator or the conductor and a recovery
//! note. Everything here works on an already rendered `config`.

use super::*;
use crate::domain::PromptText;
use crate::persistence::StepProfileEntry;

/// What [`Engine::apply_step_profile`] needs to know about the attempt.
#[derive(Clone, Copy)]
pub(crate) struct AttemptProfile {
    /// Failed attempts of the step in a row.
    pub(crate) failures: u32,
    /// Whether a rejected or escalated profile is recorded as an event. A
    /// reprompt of a rejected attempt resolves the same profile again and
    /// stays quiet: the attempt itself already recorded it.
    pub(crate) announce: bool,
}

/// The history block `action` places in its prompt: empty for an action
/// that keeps no history.
pub(crate) fn step_history(
    execution: &WorkflowExecution,
    state: &State,
    action: &ActionConfig,
) -> Result<String, EngineError> {
    match action {
        ActionConfig::Agent(config) if config.history => {
            step_memory::history_block(execution, &state.id)
        }
        _ => Ok(String::new()),
    }
}

/// Who the prompt of `action` comes from before any answer is added to it:
/// the user, too, when it embeds what they wrote.
pub(crate) fn prompt_author_of(action: &ActionConfig) -> PromptAuthor {
    match action.harness_prompt() {
        Some(prompt) if prompt.texts().any(embeds_user_content) => PromptAuthor::EngineUser,
        _ => PromptAuthor::Engine,
    }
}

impl Engine {
    /// Prepares the rendered harness action `config` of one attempt (an
    /// `agent` or a `profile` step; any other action is left alone);
    /// `template` is its prompt as the workflow declares it, `history` the
    /// step's history block.
    ///
    /// Returns the answer the attempt resumes with, consumed here, if one
    /// waited for the step.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_harness_attempt(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        template: &PromptText,
        config: &mut ActionConfig,
        history: &str,
        pinned_harness: &mut Option<String>,
        profile: AttemptProfile,
    ) -> Result<Option<AgentInput>, AttemptFailure> {
        let Some((step, prompt)) = config.harness_step_and_prompt_mut() else {
            return Ok(None);
        };
        // Before a resumed session replaces the prompt: a session that
        // goes on already knows its history (KTD9).
        step_memory::add_history(prompt, template, history);
        self.pin_harness(step, pinned_harness);
        self.apply_step_profile(execution, state, step, profile);
        let input = self.take_agent_input(execution, state)?;
        if let Some(input) = &input {
            if let (Some(harness), Some(session)) =
                (input.harness.as_deref(), input.session.as_deref())
            {
                agent::continue_session(step, prompt, harness, session, &input.answer);
                step.skip_output_contract = true;
            } else {
                prompt.push_str("\n\nHuman answer to your question:\n");
                prompt.push_str(&input.answer);
            }
            step.permission_grants = input.permission_rules.clone();
        }
        if let Some(note) = self.take_operator_recovery_note(execution)? {
            prompt.push_str("\n\nOperator recovery note:\n");
            prompt.push_str(&note);
        }
        Ok(input)
    }

    /// Sets the model and effort of a rendered step `config` from the three
    /// sources — the step's default, the entry a `profile` step stored for
    /// it, an escalation — through [`step_profile::resolve`]. A step with
    /// none of them keeps the arguments it always had.
    ///
    /// The stored profile is read at every attempt (R9): an entry written
    /// after the step was first entered applies from the next attempt on
    /// (F3). Writing it does not touch `attempt.failures` (R12).
    fn apply_step_profile(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        config: &mut HarnessStepConfig,
        attempt: AttemptProfile,
    ) {
        let default = Setting::default_of(config);
        let ceiling = Setting::ceiling_of(config);
        let entry = self.stored_step_profile(execution, state);
        let profiled = entry.as_ref().map(StepProfileEntry::setting);
        if profiled.is_none() && ceiling.is_none() {
            return;
        }
        let resolved = step_profile::resolve(ProfileInput {
            default: &default,
            ceiling: ceiling.as_ref(),
            profiled: profiled.as_ref(),
            failures: attempt.failures,
            threshold: self.escalation_threshold,
        });
        if resolved.rejected && attempt.announce {
            let by = entry
                .as_ref()
                .map_or("", |entry| entry.profiled_by.as_str());
            self.note_step_profile(
                execution,
                state,
                &format!(
                    "the profile from step '{by}' named a model or effort that cannot be \
                     used; the step's default stays"
                ),
                &default,
                &resolved.setting,
            );
        }
        if !attempt.announce {
            // Nothing more to record.
        } else if resolved.source == step_profile::ProfileSource::Escalated {
            self.note_step_profile(
                execution,
                state,
                "the step failed repeatedly; its model or effort is raised",
                &default,
                &resolved.setting,
            );
        } else if resolved.limit_reached {
            self.note_step_profile(
                execution,
                state,
                "the step failed repeatedly and already runs on its ceiling",
                &default,
                &resolved.setting,
            );
        }
        config.model = resolved.setting.model;
        config.effort = resolved.setting.effort;
    }

    /// The entry the step profile file holds for `state`, if any.
    ///
    /// A file that cannot be read — damaged, too large, of an unknown
    /// version — is "no profile" (R8): the step runs on its defaults, and
    /// the execution records it once, not at every attempt, until a
    /// `profile` step writes the file again.
    fn stored_step_profile(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Option<StepProfileEntry> {
        match self.store.read_step_profile(&execution.id) {
            Ok(stored) => stored.lookup(&state.id).cloned(),
            Err(error) => {
                tracing::warn!(%error, "unreadable step profile; running on the defaults");
                self.note_unreadable_step_profile(execution, state, &error.to_string());
                None
            }
        }
    }

    /// Records that the step profile file could not be read, unless the
    /// execution already recorded it for the file it has now.
    fn note_unreadable_step_profile(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        error: &str,
    ) {
        let noted = execution
            .metadata
            .get(METADATA_ENGINE_KEY)
            .and_then(|engine| engine.get(METADATA_UNREADABLE_STEP_PROFILE_KEY))
            .is_some();
        if noted {
            return;
        }
        if let Err(error) = self.emit(
            &execution.id,
            EventData::Error {
                message: format!(
                    "Step '{}': the step profile could not be read ({error}); \
                     the steps run on their defaults",
                    state.id
                ),
                context: json!({
                    "step_profile": { "step": state.id, "unreadable": true },
                }),
            },
        ) {
            tracing::warn!(%error, "could not record an unreadable step profile");
            return;
        }
        engine_metadata_mut(execution).insert(
            METADATA_UNREADABLE_STEP_PROFILE_KEY.to_string(),
            json!(true),
        );
        if let Err(error) = self.store.save_execution(execution) {
            tracing::warn!(%error, "could not record that the step profile was noted");
        }
    }

    /// `action` with the stored step profile applied, for a reprompt that
    /// resumes a session: the reprompt runs on the model and effort of the
    /// attempt it repairs, not on the workflow's default. An escalation is
    /// not carried over — the count of failed attempts is not kept with the
    /// rejection.
    pub(crate) fn profiled_action(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        action: &ActionConfig,
    ) -> ActionConfig {
        let mut action = action.clone();
        if let Some(step) = action.harness_step_mut() {
            self.profile_quietly(execution, state, step);
        }
        action
    }

    /// Applies the step profile to `step` without escalation and without
    /// recording anything.
    fn profile_quietly(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
        step: &mut HarnessStepConfig,
    ) {
        self.apply_step_profile(
            execution,
            state,
            step,
            AttemptProfile {
                failures: 0,
                announce: false,
            },
        );
    }

    /// Records a profile decision as an event, best-effort.
    fn note_step_profile(
        &self,
        execution: &WorkflowExecution,
        state: &State,
        message: &str,
        from: &Setting,
        to: &Setting,
    ) {
        if let Err(error) = self.emit(
            &execution.id,
            EventData::Error {
                message: format!("Step '{}': {message}", state.id),
                context: json!({
                    "step_profile": { "step": state.id, "from": from, "to": to },
                }),
            },
        ) {
            tracing::warn!(%error, "could not record a step profile decision");
        }
    }

    /// Writes the action's default harness into a rendered agent `config`
    /// that names none (R16a, R16b).
    ///
    /// The default is read from the engine's source once, at the action's
    /// first attempt, and kept in `pinned` for the rest: a default changed
    /// while the action runs reaches the next action, never a retry of this
    /// one. A harness the workflow names — templated ones included, since
    /// `config` is already rendered — always wins. An action continuing a
    /// session of its own (`resume_session`) is left alone: the executor
    /// refuses it without an explicit harness, and a default cannot say
    /// which harness that session belongs to.
    fn pin_harness(&self, config: &mut HarnessStepConfig, pinned: &mut Option<String>) {
        if config.harness.is_some()
            || config
                .parameters
                .contains_key(agent::RESUME_SESSION_PARAMETER)
        {
            return;
        }
        let harness = pinned.get_or_insert_with(|| self.default_harness.current_harness());
        config.harness = Some(harness.clone());
    }

    /// Removes and persists the note destined for the next agent attempt.
    fn take_operator_recovery_note(
        &self,
        execution: &mut WorkflowExecution,
    ) -> Result<Option<String>, AttemptFailure> {
        let Some(engine) = execution
            .metadata
            .get_mut(METADATA_ENGINE_KEY)
            .and_then(Value::as_object_mut)
        else {
            return Ok(None);
        };
        let note = engine
            .remove(METADATA_OPERATOR_RECOVERY_NOTE_KEY)
            .and_then(|value| value.as_str().map(ToString::to_string));
        if note.is_some() {
            self.store
                .save_execution(execution)
                .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        }
        Ok(note)
    }

    /// Consumes only an answer for the state currently executing. A mismatched
    /// stale record is retained instead of leaking an answer into another
    /// state after a manual transition.
    fn take_agent_input(
        &self,
        execution: &mut WorkflowExecution,
        state: &State,
    ) -> Result<Option<AgentInput>, AttemptFailure> {
        let Some(engine) = execution
            .metadata
            .get_mut("engine")
            .and_then(Value::as_object_mut)
        else {
            return Ok(None);
        };
        let Some(record) = engine
            .get(METADATA_AGENT_INPUT_KEY)
            .and_then(Value::as_object)
        else {
            return Ok(None);
        };
        if record.get("state_id").and_then(Value::as_str) != Some(state.id.as_str()) {
            return Ok(None);
        }
        let answer = record
            .get("answer")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let input = answer.map(|answer| AgentInput {
            answer,
            harness: record
                .get("harness")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            session: record
                .get("session")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            author: record
                .get("author")
                .and_then(|author| serde_json::from_value(author.clone()).ok())
                .unwrap_or(PromptAuthor::User),
            // A record of an earlier version holds one rule.
            permission_rules: match record.get("permission_rules") {
                Some(Value::Array(rules)) => rules
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect(),
                _ => record
                    .get("permission_rule")
                    .and_then(Value::as_str)
                    .map(|rule| vec![rule.to_string()])
                    .unwrap_or_default(),
            },
        });
        if input.is_some() {
            engine.remove(METADATA_AGENT_INPUT_KEY);
            self.store
                .save_execution(execution)
                .map_err(|error| AttemptFailure::permanent(error.to_string()))?;
        }
        Ok(input)
    }
}
