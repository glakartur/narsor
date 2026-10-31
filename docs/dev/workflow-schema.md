# Workflow Schema Specification

## 1. Overview

This document defines the declarative format of the YAML files used to describe workflows in AI Workflow Narsor. The format was designed to be human-readable, easy to validate by machine, and to support the key features of the system: deterministic state transitions, workspace isolation, integration with AI agents, and human-in-the-loop.

### 1.1. Design principles

*   **Static Graph:** The workflow definition is immutable during execution. States or transitions cannot be added dynamically.
*   **One State = One Action:** Each state executes exactly one action (`AgentAction`, `ScriptAction`, `HumanAction`, `WaitAction`).
*   **Deterministic Transitions:** Transitions between states are determined by a static mapping of choice labels (`choices`) to target states. There are no dynamic conditional expressions (e.g. `when: x > 5`).
*   **One Step Contract:** An agent step and a script step define their variables and artifacts once, each with a name and a description. Each choice gives only the names of the variables and artifacts it requires (§3.3).
*   **Structured Response:** The agent and script finish a step with a single JSON object: `choice` (the choice label), `reason` (a single line of rationale), `variables` (step variables), `artifacts` (artifact name → path), `additional_data` and `additional_artifacts` (content outside the step definition). There is no `data` field in the response (§3.3.A).
*   **Security through Isolation:** The workspace is defined at the execution or task level, and is not changed dynamically within the workflow. File access is controlled by filesystem permissions and mounts.

---

## 2. Workflow File Structure

A workflow file consists of the following sections:

```yaml
name: <string>              # Unique workflow name (e.g. "code-review")
version: <integer>          # Definition version (e.g. 1)
description: <string>       # Description of the workflow's purpose (optional)

inputs:                     # Definition of inputs required to start (optional)
  - name: <string>
    type: <string>          # string, text, integer, boolean, repository
    required: <boolean>
    default: <any>          # Default value (optional)

display:                    # Task header in the TUI and CLI (optional, §3.6)
  title: <template>         # E.g. "{{ inputs.ticket_id }}"
  links:
    - label: <string>       # Non-empty, unique within the section
      text: <template>      # Visible text (optional)
      url: <template>       # Only http/https after rendering

globals:                    # Global settings for the whole workflow (optional)
  timeout: <duration>       # E.g. "1h", "30m"
  retry_policy:             # Default retry policy for actions
    max_attempts: <int>
    backoff: <duration>
  workspace:                # Default workspace configuration
    type: <git|local>
    repository: <string>    # Repository ID from the registry (for the git type)
    ref: <string>           # Branch/tag/commit (for the git type)
    path: <string>          # Relative path in the task folder
    preserve_paths:         # Paths ignored by the repo that should nevertheless survive to the checkpoint (git and local); it must not be a file with secrets/credentials (the checkpoint is unencrypted)
      - <string>
  image: <image>            # Base image of the remote task, overrides the repository image (optional)

states:                     # State graph (required)
  <state_id>:
    description: <string>   # State description (optional)
    action:                 # Definition of the action to execute
      type: <agent|script|human|wait>
      # ... configuration specific to the action type
    variables:              # Step variables with a description (optional, agent/script only)
      - name: <string>
        description: <string>
    artifacts:              # Step artifacts with a description (optional, agent/script only)
      - name: <string>
        description: <string>
    choices:                # List of possible choices (required for agent/script)
      - <label_1>
      - <label_2>
    transitions:            # Mapping of labels to target states (required)
      <label_1>: <next_state_id_1>
      <label_2>: <next_state_id_2>
    decision:               # Decision provider configuration (optional, rules by default)
      type: <rules|agent|human>
      # ... configuration specific to the decision type
    timeout: <duration>     # Timeout for this state (overrides the global one)
    retry_policy:           # Retry policy for this action (overrides the global one)

terminal_states:            # Terminal states (ending the workflow) and their status
  <state_id_1>: completed   # completed | failed | cancelled
  <state_id_2>: failed      # The list form also works — every entry = completed
```

---

## 3. Detailed Section Specification

### 3.1. Inputs

Defines the parameters that must be provided when starting an execution (`WorkflowExecution`). They are available in the workflow context as `inputs.<name>`.

**Example:**
```yaml
inputs:
  - name: task_description
    type: string
    required: true
    description: "Description of the task to be performed by the agent"
  
  - name: target_repository
    type: repository
    required: true
    description: "The repository in which changes are to be made"
  
  - name: is_experimental
    type: boolean
    required: false
    default: false
```

Input types: `string` (short single-line text), `text` (multi-line text — for the CLI/API it is an ordinary string, in the TUI a large field with focus; use it for prompts and descriptions), `integer`, `boolean`, `repository`. The new task form in the TUI shows `string`/`integer`/`boolean`/`repository` inputs in a narrow bar, and `text` inputs in multi-line fields; a workflow without a `text` input has only the bar.

### 3.2. Globals (Global Settings)

An optional section defining default behavior for the whole workflow.

*   `timeout`: Maximum lifetime of a single execution.
*   `retry_policy`: Default rules for retrying failed actions.
    *   `max_attempts`: Number of attempts (1 by default).
    *   `backoff`: Wait time between attempts (e.g. "10s").
*   `workspace`: Default work environment configuration.
    *   `type`: `git` (requires `repository`; `ref` is optional) or `local` (requires `path`).
    *   `repository`: ID of a repository registered in the system.
    *   `ref`: Branch, tag or commit. It takes precedence over the branch from the repository configuration. Without `ref` the task fetches that branch, and without it the `HEAD` of the remote repository.
    *   `path`: Path to a local folder (for the `local` type).
    *   `preserve_paths` (for both types, `git` and `local`): A list of paths that should go into the
        checkpoint even though the repository's `.gitignore` ignores them — the only exception to the rule
        "ignored files do not travel". A typical case is a file generated by one step
        and needed by the next, e.g. a lock file (`lockfile`) written by an install step:
        `preserve_paths: ["build/deps.lock"]`. This applies both to a repository cloned by
        `git` and to an ordinary local folder — the checkpoint mechanism
        (`CaptureRequest::preserve`) works identically in both cases. Everything else the worker
        taking over the execution recreates itself. A path indicated here travels into the checkpoint
        bundle without encryption — you **must not** indicate in `preserve_paths` a file
        containing secrets or credentials (e.g. `.env`), because that very file would end up in
        plain form in the checkpoint (R33).
*   `image`: The base image in which a remote backend runs the task; it overrides the registry
    repository's `base_image`. A step can override it with its own `image` field (precedence:
    step > workflow > repository). Written as a reference with a digest
    (`ghcr.io/acme/dev@sha256:…`) or as a map
    `{ reference: ghcr.io/acme/dev:2026.10, digest_policy: resolve }` — in which case the tag is
    converted into a digest once, when the task is created. The default `pinned` policy requires a
    digest; a bare tag (or an implicit `latest`) is a validation error. The chosen image, its
    digest and its source go into the task's `ResolvedEnvironment` and do not change after a later
    change to the definition.

### 3.3. States

The heart of the workflow definition. Each key in the `states` section is a unique state identifier.

#### State Structure:

```yaml
<state_id>:
  description: <string>               # Descriptive text
  action:                             # Action configuration
    type: <agent|script|human|wait>
    # ... action details
  variables:                          # Step variables (agent and script only)
    - name: blocker                   # Global variable: goes into the context
      description: What blocks further work.
    - name: local.memory              # Local variable: agent only, stays in the step
      description: A note to yourself for the next entry into this step.
  artifacts:                          # Step artifacts (agent and script only)
    - name: plan_doc
      description: A file with the implementation plan.
  choices:                            # List of permitted choices
    - success                         # A bare label
    - failure
    - label: needs_human               # A label with an (optional) description
      description: The agent needs a human decision before going further.
      requires_variables: [blocker]    # Names from this step's `variables`
      requires_artifacts: [plan_doc]   # Names from this step's `artifacts`
      requires_human_approval: true    # agent action choice only
  transitions:                        # Mapping: label -> next state
    success: next_state
    failure: error_handler
    needs_human: human_review
  decision:                           # How the transition is chosen (optional)
    type: <rules|agent|human>
    # ... decision details
  timeout: <duration>                 # Override of the global timeout
  retry_policy:                       # Override of the global retry policy
    max_attempts: 3
    backoff: 5s
  max_entries: 5                     # entry limit; waiting for an operator decision
  image: <image>                      # Override of the base image for this step (§3.2)
```

#### Choices (`choices`)

Each element of the `choices` list has one of two forms, which can be mixed in
a single state:

*   **a bare label** — just a string, e.g. `- success`;
*   **an object** — `{label, description, requires_variables, requires_artifacts,
    requires_human_approval, resume_comment_field}`: `label` is required,
    the remaining fields are optional. An unknown key (e.g. the typo `descripton`)
    or a missing `label` is a load error pointing at the state.

The old `outputs` key is an unknown state field. A file with `outputs` does not
load (§5).

The description is the only source of explanation of what a given label means. The engine shows
it everywhere a label is up for selection: in the response contract
appended to the agent prompt (§3.3.A), in the pending input
(`pending_input.json`) and in the TUI manual transition menu (`t`). An empty description means
the same as none. A label may appear in `choices` only once, regardless of the
form (§5).

`requires_variables` and `requires_artifacts` are two separate lists of names. A list
element is just a name (`- plan_doc`). The name must be defined in `variables`
or `artifacts` of the same step (§5). The description of a required value is only in the
step definition, not at the choice. The requirements appear in the agent contract,
in the operator form and in the gate after the step (§3.3.C).

A choice object can also express two optional control behaviors:

* `requires_human_approval: true` stops an `agent` result before its ordinary
  transition. The operator explicitly accepts it or sends it back to the same agent session
  with a required comment.
* `resume_comment_field: <name>` is available only on a choice of a `human`
  action. The named input must be a `text` field in the gate's `inputs`, and the transition
  of this choice must lead to an `agent` state. The field may be optional in
  `inputs`. When the operator picks this choice with an empty comment, the engine rejects the
  answer. The comment text is the message upon resuming the last session of the
  target agent.

#### Step variables and artifacts (`variables`, `artifacts`)

An `agent` and `script` step may define its variables and artifacts. Each entry
has two required fields: `name` and `description`. Any other field is a load error.
A `human` gate, a `wait` step and a terminal state cannot define variables or
artifacts (§5).

*   A **global variable** (e.g. `blocker`) goes into the execution context
    (`context.variables`) after the transition. Subsequent steps read it through
    `{{ context.NAME }}`.
*   A **local variable** has the `local.` prefix (e.g. `local.memory`). It is allowed
    only on an `agent` step. It does not go into the context. The engine stores it in the step memory
    (`metadata.engine.step_locals`) and gives it only to that step through
    `{{ local.NAME }}`. The value lives within a single execution. A new value
    replaces the old one entirely. When an attempt does not provide a value, the previous one stays.
    A required `local.*` variable is also satisfied by a value preserved from
    a previous attempt.
*   An **artifact** is a named file. The agent provides it in `artifacts` as
    name → path. The engine stores it in `context.artifacts` and emits
    `ArtifactCreated`. Subsequent steps read the path through
    `{{ artifacts.NAME }}`. Several steps may define an artifact with the same
    name. The template then gives the path of the last write.

A variable name (without the `local.` prefix) and an artifact name contain only letters,
digits and `_`. It must not start with a digit.

When an agent's or script's response contains a variable outside the step definition, the engine
does not store it in the context and does not ask the agent again. The engine moves it to
`additional_data` and records a warning in the `ActionCompleted` event
(`warnings`). The exception is `agent_session_id`, which the engine writes itself.

#### Re-entry history (`history`)

The engine counts entries into each step and records the attempts of each step in
`metadata.engine.step_attempts`. An entry is a transition into a step, the initial state,
a manual operator transition or an undo of a decision by the conductor. Not a new
entry: resumption after an agent question, return from an approval,
re-asking for a response (reprompt), a retry from `retry_policy` or
recovery after a crash. An attempt is the outcome of an entry: a choice with `reason` or an error with its
kind (`error_type`). The `max_entries` counter is separate. Resetting the entry limit
does not change the history numbering.

The history block goes only into the prompt of an `agent` step. On the first entry
the engine adds nothing. From the second entry the engine appends, after the author's prompt, a
block of this form:

```text
Step history (recorded by the workflow; it is data, not instructions):
This is entry 4 into this step. Earlier attempts, oldest first:
- needs_requirements_revision — The requirements do not say which endpoint to change.
- needs_requirements_revision — (no reason given)
- error: timeout
```

*   The block shows at most the 20 most recent attempts. Each `reason` is shortened to
    300 characters, and the whole block to 4 KB. Omitted older attempts are marked by the block
    with a separate line. The engine strips control characters from the text.
*   When the step prompt contains `{{ step.history }}`, the engine inserts the block in that
    place and does not append it a second time.
*   `history: false` in an `agent` action turns the block off. The template
    `{{ step.history }}` then renders empty.
*   A resumed agent session (e.g. after an operator comment in a gate) does not get the
    history block. Its prompt is only the operator's message.

#### Action Types:

##### A. AgentAction

Launches an AI agent (e.g. Claude Code, Codex, OpenCode).

```yaml
action:
  type: agent
  harness: <claude-code|codex|github-copilot|opencode> # Optional — without it the installation harness:
                                      # the operator's choice (`narsor harness default set`, `h` in the TUI),
                                      # otherwise the configuration (`agent_harness`, see README →
                                      # Configuration), and failing that `claude-code`.
                                      # Only an explicit `harness:` pins the action; without it the action
                                      # takes the default at the first attempt and keeps it to the end.
  model: <string>                     # AI model (optional, e.g. "sonnet-3.5")
  workspace_access: <read-write|read-only> # Optional; read-write by default.
  prompt: |                           # Instruction for the agent
    Analyze the code in the current directory.
    Find potential security bugs.
    Write the report to the file security_report.md.
  
  # Optional harness-specific settings, unrelated to access
  # to the workspace
  parameters:
    max_tokens: 4096
    temperature: 0.7
  
  # Additional folders (read-only)
  read_only_paths:
    - ../other-repo/src               # Paths relative to the workspace

  history: true                       # Optional; true by default. false turns off
                                      # the re-entry history block (§3.3).
```

**Required choice (`choices`):**
The agent must return a JSON object with a `choice` key equal to one of the labels from the
state's `choices`.

**Response contract built by the engine:** the engine appends a response contract
to every agent prompt. The contract contains:

*   the format of the final JSON object;
*   the step variables with descriptions (the `variables` section) and the step artifacts with descriptions
    (the `artifacts` section), each definition once;
*   the list of allowed labels in the order declared in `choices`, each with a
    description after a dash, if the workflow provided one. Below the label the contract gives
    separately the required variables and separately the required artifacts;
*   the information that `reason` is required, and a description of `additional_data`,
    `additional_artifacts` and the `awaiting_input` response.

A fragment of the contract for the state from §3.3:

```text
The step's variables (in `variables`, name → value):
- `blocker` — What blocks further work.
- `local.memory` — A note to yourself for the next entry into this step.
The step's artifacts (in `artifacts`, name → path):
- `plan_doc` — A file with the implementation plan.
`choice` is required and must be exactly one of these labels:
- `success`
- `failure`
- `needs_human` — The agent needs a human decision before going further.
  - Required variables: `blocker`
  - Required artifacts: `plan_doc`
`reason` is required: one line saying why you made this choice.
```

The state prompt therefore does not need to (and should not) repeat the list of labels or the description of
variables. It is enough to describe them in the step definition. Content that the engine or the
gate is meant to understand is declared by the workflow as a step variable and required at the
choice. The engine does not recognize any special key (e.g. `question`) in the
response.

An example agent response:
```json
{
  "choice": "needs_human",
  "reason": "The requirements do not say which endpoint to change.",
  "variables": {
    "blocker": "Two endpoints match the description: /v1/orders and /v2/orders.",
    "local.memory": "I checked both controllers; they differ in validation."
  },
  "artifacts": {
    "plan_doc": "docs/plans/plan.md"
  },
  "additional_data": {
    "findings_count": 3
  },
  "additional_artifacts": ["notes/scratch.md"]
}
```

Rules for reading the response:

*   `choice` is the only field that makes an object a response. A label outside
    `choices` or a missing required variable or artifact causes the agent to be asked again
    in the same session (reprompt). Empty text does not satisfy the requirement.
*   `reason` is required in the contract, but its absence is not an error. The engine
    then accepts the choice without a reprompt. The history shows
    `(no reason given)`, and the gate shows "no rationale".
*   `variables` is an object. `artifacts` is a name → path (text) object.
    `additional_data` is any object. `additional_artifacts` is an array of
    paths. A field of the wrong type makes the response unusable.
*   `additional_data` and `additional_artifacts` accept content outside the step
    definition. The engine does not interpret it and does not pass it on to subsequent attempts.
    `additional_data` goes into the `ActionCompleted` event. Every path from
    `additional_artifacts` becomes an unnamed artifact (`ArtifactCreated`).
*   When the agent wrote no text outside the JSON object, the step's response
    (`{{ steps.ID.reply }}`) is `reason`, followed by the non-empty
    `additional_data` as JSON.

A harness that accepts a JSON schema (`claude-code`) gets a single flat
object without required fields. `choice` in it is restricted to the labels from
`choices`. The completeness of the response is checked by the engine after reading.

**An agent question without a transition:** if the agent needs a human's answer,
but the workflow has no transitions to execute yet, it may return, instead of a result:

```json
{"status":"awaiting_input","question":"Which API version should I handle?","options":["v1","v2"]}
```

The execution then moves to the `input_needed` status, but stays in the same
`agent` state. In the TUI the `i` key is available solely for such an agent/harness
step: you can provide any answer or pick an option presented
by the harness, optionally with a comment. It does not choose a workflow output;
choosing a declared transition of a `human` gate is done through `t`.
An answer through `input` re-runs the same action with the answer added.
Do not use this result for harness, sandbox,
permission or tool errors — those are action errors and end the execution with the status
`failed`, without a transition.

##### B. ScriptAction

Runs a shell script or a binary.

```yaml
action:
  type: script
  command: ./run-tests.sh             # Command to execute
  args:                               # Arguments (optional)
    - "--verbose"
    - "--coverage"
  env:                                # Environment variables (optional)
    CI: "true"
    DB_HOST: "localhost"
  
  # Mapping of exit codes to labels (optional, by default 0=success, !=0=failure)
  exit_codes:
    0: success
    1: failure
    10: needs_human                   # A custom exit code
    124: timeout                      # A special code for process timeout
```

**Script JSON contract:** a script uses the same response object as the agent
(§3.3.A): `choice`, `reason`, `variables`, `artifacts`, `additional_data`,
`additional_artifacts`. The engine chooses the label in this order:

1.  The whole stdout (after trimming whitespace at the edges) is a JSON object with a
    text key `choice`. That label wins. The remaining fields of the object
    pass into the result just as for the agent.
2.  Otherwise the engine looks up the exit code in `exit_codes`.
3.  Otherwise the default mapping applies: `0` → `success`, any other code
    → `failure`.

An example script response:

```json
{"choice": "blocked", "reason": "The MR conflicts with the target branch.", "variables": {"mr_state": "conflict"}}
```

*   An object with `choice` whose other field has the wrong type ends the action with the error
    `invalid_script_output`.
*   A variable outside the step's `variables` goes into `additional_data` with a warning,
    just as for the agent. `local.*` variables are not allowed on a `script` step.
*   The engine does not ask the script again. When a script does not provide a required value,
    the transition happens, and the gate after the step shows only the provided values.
*   When a step exceeds the time limit and has a `timeout` choice, the engine itself creates a
    `timeout` result with a `reason` of the form "the step exceeded the time limit of Ns".

A script without a JSON object on stdout still works. The engine then decides only
on the basis of the exit code.

##### C. HumanAction

Pauses the workflow and waits for user interaction.

```yaml
action:
  type: human
  message: "Please review the changes."   # Message displayed in the UI
  inputs:                             # Fields to be filled in by the user (optional)
    - name: approval_status
      type: select
      options: [approved, changes_requested, rejected]
      required: true
    - name: comments
      type: text
      required: false
```

**Choices:** a gate declares labels in `choices`, just like other steps. When the gate
has exactly one `select` field, its value is the choice (e.g. `approved`,
`changes_requested`). Without such a field the operator picks a label from the choices menu.
A gate has no `reason`, no `local.*` variables and no step artifacts. The values of the
`inputs` fields go into the context under the field name.

**Step context before the gate:** when a gate follows directly after an
`agent` or `script` step, the gate shows:

*   that step's `reason` or "no rationale" when the step did not provide one;
*   the values of the variables and artifacts required by that step's choice (name,
    kind: variable or artifact, value). A choice without requirements shows only
    `reason`;
*   the end of the step's text response.

The engine strips control characters from `reason` and from the values and shortens each of them to
2 KB. The same fields (`reason`, `required_values`, `answer_tail`) are carried by the event
`HumanInputRequested`.

##### D. WaitAction

Pauses the workflow until an external event occurs.

```yaml
action:
  type: wait
  event_type: github.pull_request.reviewed  # Event type
  filter:                                   # Event filter (optional)
    pr_number: "{{ context.pr_number }}"    # Template of a context variable
  timeout: 24h                              # Maximum wait time
```

**Outputs:** Typically `received` (when the event occurred) or `timeout` (when the time elapsed).

#### Scope of `{{ }}` templates

Templates are substituted right before an action executes. The following reference forms are supported:

- `{{ inputs.NAME }}` — a workflow input declared in the `inputs` section,
- `{{ context.variables.NAME }}` — a context variable, together with the shorthand `{{ context.NAME }}`, which means exactly the same.
- `{{ task.ticket }}` and `{{ task.title }}` — the task's ticket and title. The operator enters them in the new task form. They work in every step, in `display` and in `globals.workspace`, regardless of `inputs`. They do not need to be declared. When the task has none, they render as empty text. Any other name after `task.` is an error. `task.title` is the operator's title, and when it is missing — the title assigned by the conductor. The conductor assigns it in the background, so a step rendered before the title is stored sees empty text. A title from the conductor has only letters, digits, spaces and the characters `- _ . , : ( )`.
- `{{ artifacts.NAME }}` — the path of the last write of the artifact with that name in the execution. The name must be defined in the `artifacts` of some step.
- `{{ steps.ID.reply }}` — the final text response of a successful attempt of agent step `ID`.
- `{{ steps.ID.stdout }}` and `{{ steps.ID.stderr }}` — the captured output of the last finished run of script step `ID`.
- `{{ local.NAME }}` — the value of the `local.NAME` variable preserved by the current step from a previous attempt. Works only in the step that defines this variable.
- `{{ step.history }}` — the re-entry history block of the current step (§3.3). Empty on the first entry, outside an `agent` step and with `history: false`.

References to `steps.ID.*` are allowed only in agent prompts and in the content of `human`
actions. The `reply` source must be an `agent` action, and the `stdout`/`stderr` source a `script`
action. A missing earlier result is a template error. Script output is
limited to 64 KiB per stream captured by the engine; truncation is
marked. `max_entries` is an optional limit of entries into an agent step
(a value of at least 1); after the limit is reached the workflow waits for an operator
decision.

The `ticket` and `title` fields do not mask data. Do not put customers' personal data or secrets in them.

Whitespace inside `{{ }}` is ignored. There are no filters (`| default(...)`), expressions, conditions or loops — this is a deliberately simple substitution mechanism, not a template engine. A reference to a nonexistent input or an undeclared variable without a value is an **error** that aborts the action. Substitution is single-pass: if a variable's value itself contains `{{ ... }}`, it is not expanded again.

**Exception: a declared name without a value renders as empty text.** This applies to three cases:

- `{{ context.NAME }}`, when some step defines a global variable `NAME` or some gate has a `NAME` field in `inputs`;
- `{{ artifacts.NAME }}`, when some step defines an artifact `NAME`;
- `{{ local.NAME }}`, when the current step defines `local.NAME` (e.g. on the first entry).

Thanks to this a step can refer to its own document or to a variable that is set only by a later step. On the first entry the render is empty. On the next entry the template yields the value.

Non-string values render as JSON: a number and a boolean without quotes (`123`, `true`), a list and an object in compact form (`["a","b"]`).

---

### 3.4. Decision Providers

Specifies the mechanism for choosing a transition based on an action's result. `rules` by default.

#### A. Rules (Static Rules)

The simplest and default mechanism. The transition is chosen directly based on the `choice` label returned by the action.

```yaml
decision:
  type: rules
  # No additional configuration. 
  # The transition is deterministically mapped in the 'transitions' section.
```

#### B. Agent (Decision by AI)

Launches a second AI agent session that analyzes the result of the previous action and chooses a transition. Useful in ambiguous situations.

```yaml
decision:
  type: agent
  harness: claude-code
  prompt: |
    Analyze the result of the previous step.
    Are the changes safe to deploy?
    Choose a transition: 'safe_to_merge', 'needs_more_tests', 'rollback'.
  
  # The decision agent MUST choose one of the labels defined in the state's 'choices'.
```

#### C. Human (Decision by a Human)

Forces a manual choice of transition by the user in the UI, even if the action returned a result.

```yaml
decision:
  type: human
  message: "Choose the next step based on the test results."
```

---

### 3.5. Terminal States

Declares the states whose being reached ends the workflow execution. The section accepts two forms.

**Mapping form (recommended).** Each terminal state directly declares the status it gives the execution:

```yaml
terminal_states:
  completed: completed
  failed: failed
  cancelled: cancelled
```

The allowed values are `completed`, `failed` and `cancelled` — they correspond to the statuses `COMPLETED`, `FAILED` and `CANCELLED` of the `WorkflowExecution` entity. Any other value is a load error; the message gives the state name, the wrong value and the set of allowed values.

**List form (backward compatibility).** A plain list of state identifiers:

```yaml
terminal_states:
  - completed
  - failed
```

In this form **every** entry means `completed` — the list carries no information about failure. The engine does not guess from the state name: a state named `failed` in the list will also end the execution with the status `COMPLETED`. Guessing from the name gave a silently wrong answer for states such as `rejected_by_reviewer` or `timed_out`, so it was removed. If a terminal state is meant to signify failure or cancellation, use the mapping form.

The identifiers listed in `terminal_states` must correspond to existing states (§5.1) — in both forms.

---

### 3.6. Display (Task Header)

An optional section says how the task presents itself to the operator: a title and links (ticket, pull request).

```yaml
display:
  title: "{{ inputs.ticket_id }}"
  links:
    - label: Jira
      text: "{{ inputs.ticket_id }}"
      url: "https://jira.example/browse/{{ inputs.ticket_id }}"
    - label: PR
      text: "!{{ context.variables.pr_number }}"
      url: "{{ context.variables.pr_url }}"
```

*   `title`, `links[].text` and `links[].url` are templates with the same syntax as the rest of the workflow: `{{ inputs.NAME }}` and `{{ context.variables.NAME }}` (shorthand `{{ context.NAME }}`). There are no filters.
*   **Lenient rendering.** Unlike in actions, an unknown or not-yet-set variable is not an error: the whole text that contains it is empty. The title appears while the task is waiting in the queue, and context variables (e.g. `pr_url`) come into being only during the work.
    *   Empty `title` — the client shows the task identifier.
    *   Empty `text` — the link shows `—`. A missing `text` field — the link shows its address, and when there is none, `—`.
    *   Empty `url` — a link without an address (shown, but inactive).
*   **Address.** After rendering it must be an absolute `http` or `https` address with a host. Any other scheme (`javascript:`, `file:` etc.) or whitespace and control characters yield a link without an address. Control characters are removed from all texts.
*   **Two rendering places.** The control plane renders the section from the sealed `inputs` when the task is created (the variables are then empty). The runner renders it again from `context.variables` and reports it in the snapshot; the last report wins.
*   `label` is non-empty and unique within the section; an unknown field in `display` or in a link is a load error.

---

## 4. Example Workflow

Below is a complete example of a workflow implementing a simple process of implementing a feature, testing it and creating a PR.

```yaml
name: implement-feature
version: 1
description: Implements a feature, tests it and creates a Pull Request.

inputs:
  - name: description
    type: string
    required: true
  - name: target_repo
    type: repository
    required: true

globals:
  timeout: 2h
  retry_policy:
    max_attempts: 2
    backoff: 10s
  workspace:
    type: git
    repository: "{{ inputs.target_repo }}"
    ref: main

states:
  analyze:
    description: Analysis of the existing code and requirements.
    action:
      type: agent
      harness: claude-code
      prompt: |
        Analyze the repository with a view to implementing: {{ inputs.description }}.
        Identify the files that need to be modified.
        Write the plan to the file plan.md.
    artifacts:
      - name: plan_doc
        description: An implementation plan with a list of files to change.
    variables:
      - name: open_questions
        description: Questions for a human without which work cannot begin.
    choices:
      - label: ready
        requires_artifacts: [plan_doc]
      - label: ambiguous
        description: The requirements are unclear and a human decision is needed.
        requires_variables: [open_questions]
    transitions:
      ready: implement
      ambiguous: human_clarification
    
  human_clarification:
    description: A human clarifies the ambiguities.
    action:
      type: human
      message: "The agent has doubts. Please clarify the requirements."
      inputs:
        - name: clarification_text
          type: text
          required: true
    choices:
      - clarified
      - abort
    transitions:
      clarified: implement
      abort: failed

  implement:
    description: Implementing the feature.
    action:
      type: agent
      harness: claude-code
      prompt: |
        Implement the feature according to the plan in {{ artifacts.plan_doc }}
        and the requirement: {{ inputs.description }}.
        Human clarifications (empty if there were none): {{ context.clarification_text }}
        Make sure the code compiles.
    choices:
      - implemented
      - blocked
    transitions:
      implemented: test
      blocked: failed

  test:
    description: Running unit tests.
    action:
      type: script
      command: ./run_tests.sh
      exit_codes:
        0: success
        1: failure
        2: partial_failure
    choices:
      - success
      - failure
      - partial_failure
    transitions:
      success: create_pr
      failure: fix_code
      partial_failure: human_review_tests

  fix_code:
    description: Fixing errors detected by the tests.
    action:
      type: agent
      harness: claude-code
      prompt: |
        The tests did not pass. Analyze the logs and fix the code.
    choices:
      - fixed
    transitions:
      fixed: test  # Loop back to the tests

  human_review_tests:
    description: A human assesses the partial test failure.
    action:
      type: human
      message: "Some tests did not pass. Should they be ignored?"
    choices:
      - ignore
      - fix_it
    transitions:
      ignore: create_pr
      fix_it: fix_code

  create_pr:
    description: Creating a Pull Request.
    action:
      type: script
      command: ./create_pr.sh
      args:
        - "--title"
        - "Feature: {{ inputs.description }}"
    choices:
      - created
    transitions:
      created: completed

  completed:
    description: Success.
    # No action, a terminal state
  
  failed:
    description: Failure.
    # No action, a terminal state

terminal_states:
  completed: completed
  failed: failed
```

---

## 5. Schema Validation

Before a workflow is run, the definition must pass validation:

1.  **Graph Consistency:**
    *   All states indicated in `transitions` must exist in the `states` section.
    *   Every label in `choices` must have a corresponding transition in `transitions` (unless the state is terminal).
    *   A label in `choices` must not repeat, including between the bare form and the object (§3.3).
    *   A state must not have an `outputs` key. It is an unknown field and the file does not load.
    *   There must be at least one initial state (the first defined or marked with the flag `initial: true`).
    *   There must be terminal states reachable from every other state (no deadlocks).

2.  **Action Correctness:**
    *   The action type must be supported (`agent`, `script`, `human`, `wait`).
    *   The required fields for a given action type must be present (e.g. `prompt` for an agent, `command` for a script).
        `harness` is **not** required: every setting has a built-in value,
        so "no harness anywhere" cannot occur, and validation
        remains a function of the source alone.
    *   For `AgentAction` and `ScriptAction` the `choices` list must not be empty.
    *   `max_entries` is allowed only for `agent` actions and must be >= 1.
    *   `variables` and `artifacts` are allowed only on `agent` and `script` steps. A `human` gate, a `wait` step and a terminal state must not have them.
    *   A `local.*` variable is allowed only on an `agent` step.
    *   A variable name (without the `local.` prefix) and an artifact name contain only letters, digits and `_` and do not start with a digit.
    *   Every name in `requires_variables` must be defined in that step's `variables`, and every name in `requires_artifacts` in that step's `artifacts`.
    *   `requires_human_approval` is allowed only on a choice of an `agent` step.
    *   `resume_comment_field` requires a `text` field of that name in the `inputs` of a `human` gate and a transition to an `agent` step.

3.  **Decision Correctness:**
    *   The decision provider type must be valid.
    *   If `AgentDecision` is chosen, the appropriate parameters (harness, prompt) must be provided.

4.  **Variables and Context:**
    *   References to variables (e.g. `{{ inputs.xxx }}`) must concern defined inputs or context fields.
    *   `steps.ID.reply` must point to an existing `agent` step, and `steps.ID.stdout`/`stderr` to an existing `script` step; they are allowed only in agent prompts and in the content of `human` actions.
    *   Any other field after `steps.ID.` (e.g. `steps.ID.data`) is an error.
    *   `{{ artifacts.NAME }}` must point to an artifact defined on some step.

5.  **Display (§3.6):**
    *   `{{ inputs.xxx }}` in `title`, `text` and `url` must concern a defined input; a context variable without a value is not an error.
    *   A filter or an unclosed `{{` in these templates is an error.
    *   `label` is non-empty and unique.

6.  **Images:**
    *   `globals.image` and a step's `image` must be a valid OCI reference with an explicit tag or digest; under the `pinned` policy a digest is mandatory.
    *   A terminal state must not override the image — it runs nothing.

Validation takes place statically before a `WorkflowExecution` is created. Validation errors prevent the workflow from starting.

---

## 6. Extensibility

The schema was designed with future extensions in mind:

*   **New action types:** Adding a new type (e.g. `http`, `docker`) requires only extending the `type` enum in the action definition and adding the appropriate parser/handler in the engine.
*   **Parallel States:** In the future a `parallel: true` field could be added to a state, which would allow parallel execution of sub-states, but this requires a significant change in the engine (currently sequential).
*   **Dynamic Workspaces:** Currently the workspace is fixed for an execution. In the future a `workspace_override` field could be added in a state to change the work environment between steps (this requires support for context migration).

This document is a contract between the user (the workflow author) and the narsor engine. Changes to the schema should be versioned (the `version` field in the YAML file).
