---
title: "Output contract once per session: a runtime-only marker set by the one branch that knows the case"
date: 2026-11-12
category: design-patterns
module: engine/actions/agent
problem_type: design_pattern
component: tooling
severity: medium
applies_when:
  - "The engine appends a fixed block (such as the output contract) to every prompt, and some prompts go to a session that already holds it"
  - "Several code paths resume a session, and only one of them should change the prompt"
  - "A flag must change behaviour at run time but must not enter the workflow source or the saved definitions"
tags: [output-contract, resume-session, operator-answer, harness-step-config, serde-skip, reprompt, prompt]
---

# Output contract once per session: a runtime-only marker set by the one branch that knows the case

## Context

Every agent prompt ended with the `output_contract` block. A session resumed with the
operator's answer in the same step already held that block from its first prompt. The
operator saw the block in the conversation on every own message, and the agent paid
tokens for it each time.

The parameter `resume_session` could not tell the cases apart. Three paths set it:
the operator's answer, the repair of the format (engine and conductor), and
`resume_session` from the workflow. Only the first one must drop the contract.

Sources: PR 99, plan `docs/plans/2026-11-12-001-feat-kontrakt-wyjscia-raz-na-sesje-plan.md`,
requirements `docs/brainstorms/2026-11-12-kontrakt-wyjscia-przy-wznowieniu-sesji-requirements.md`.

## Guidance

1. **Put a marker in the step config, not a rule in the executor.**
   `HarnessStepConfig.skip_output_contract` is `#[serde(skip)]` and `false` by default
   (same model as `permission_grants`). It never enters the workflow source or a saved
   definition. The default keeps the old behaviour, so first prompts and workflow
   `resume_session` need no change.
2. **Set the marker in the one branch that knows the case.**
   `prepare_harness_attempt` sets it only when `input.harness` and `input.session` are
   both present (the operator returns with a saved session). `continue_session` does
   not set it, because the repair of the format uses `continue_session` too.
3. **Let the executor only read the marker.**
   `AgentExecutor::run` builds `prompt + repositories + contract`. The contract is an
   empty string when the marker is on.
4. **Reset the marker where the contract must come back.**
   `reprompt_config` sets `skip_output_contract = false` after `continue_session`.
   The repair takes its action from an earlier attempt, and that action may carry the
   marker. The reset gives the contract back whatever the source.
5. **Leave the "no session" branch alone.** An operator answer without a saved session
   starts a new session. It keeps the contract.

The conversation (TUI and API) stores the prompt sent to the agent. One change to the
prompt therefore also changes what the operator sees. No separate UI change is needed.

## Why This Matters

- A marker that is off by default cannot remove the contract by accident.
- One setter plus one reset makes the rule short to test and to read.
- A flag in the saved definition would leak run state into persisted data and into
  the reading of older runs. `serde(skip)` prevents that.

## When to Apply

- A fixed prompt block may be redundant on a resumed session.
- A shared helper (`continue_session`) serves several callers with different needs.
  Do not put the case-specific change in the helper. Put it in the caller.

## Examples

Executor (`agent.rs`):

```rust
let contract = if config.harness_step.skip_output_contract {
    String::new()
} else {
    output_contract(&request.contract, harness.contract_note())
};
let prompt = format!("{}{}{}", config.prompt, repositories, contract);
```

Operator branch (`harness_attempt.rs`):

```rust
agent::continue_session(step, prompt, harness, session, &input.answer);
step.skip_output_contract = true;
```

Repair (`contract.rs`):

```rust
continue_session(step, prompt, harness, harness_session, message);
step.skip_output_contract = false; // a repair of the format needs the contract
```

Tests (`tests/integration/actions_agent.rs`, `conductor_chat.rs`) cover AE1–AE4: first
prompt keeps the contract, operator resume drops it, format repair keeps it, workflow
`resume_session` keeps it. Another test checks that the marker is not serialized.
The PR description states that the test targets passed locally. This doc did not re-run them.

## Open risk

The code gives no proof that every harness other than Claude Code keeps the first
prompt in a long session. The rule applies to all harnesses. The fallback is the
repair of the format with the contract (R3). After rollout, check how often a repair
follows an operator resume. A person must confirm the result before the rule is
narrowed or kept.
