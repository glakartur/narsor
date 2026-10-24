# Shipped workflows

Four runnable pipelines, written against `WORKFLOW_SCHEMA.md` and modelled on
real agentic development processes rather than invented ones:

| File | Process | Source material |
|---|---|---|
| `compound-engineering.yaml` | Five stages on one Draft pull request (merge request on GitLab): requirements → plan → implementation → learning → finalize (the merge request leaves Draft; a person merges it), with a person at the end of each stage | the `ce-*` skills of the `compound-engineering` plugin, and `gh` (GitHub) or `glab` (GitLab) |
| `compound-engineering-chat.yaml` | The same five stages. Copy of `compound-engineering.yaml` in which `brainstorm` and `plan` talk to the person: they ask one question at a time in the chat window (`awaiting_input`) and do not push the questions into the document | `ce-brainstorm` and `ce-plan`, with the chat contract in the prompt |
| `compound-engineering-debug.yaml` | The `ce-debug` flow: diagnosis → gate (fix, revise, diagnosis only) → test-first fix → review and checks → Draft merge request → review gate → learning → finalize. The agent asks in the chat window | `ce-debug`, `ce-code-review`, `ce-compound` |
| `open-mercato-sdlc.yaml` | The Open Mercato ticket lifecycle: Discovery → Intake → Triage → Claim → Design → Implement → PR → review loop → QA → Merge → housekeeping | [`SDLC.md`](https://github.com/open-mercato/skills) and the `om-*` skills |

All validate with zero violations. The two original pipelines are exercised end to end in
`tests/workflows.rs` against a fake harness and no real
`claude`, `git`, `gh` or `glab` is ever run by the tests.

## Chat variants

`compound-engineering-chat.yaml` and `compound-engineering-debug.yaml` ask
the person questions in the chat window. The agent answers with
`{"status":"awaiting_input","question":"...","options":[...]}`. The execution
waits in `input_needed`; the person answers with `i` in the TUI or with
`narsor input`, and the engine runs the same step again in the same agent
session. The prompts forbid a question tool such as `AskUserQuestion`, which
cannot work without a terminal. Timeouts are longer than in the original
(`globals.timeout: 12h`, 3h for the dialogue steps).

`compound-engineering-debug.yaml` has these gates: `review_diagnosis` (`fix`,
`revise`, `diagnosis_only`, `abandon`), `review_fix` (`check_feedback`,
`accept`, `revise`, `revise_diagnosis`, `abandon`), `review_learning` and
`finalize_blocked`. `diagnosis_only` ends the run in `diagnosed_only`
(completed) and publishes nothing; the diagnosis stays in the history of the
`diagnose` step. The fix step goes back to `diagnose` with `diagnosis_blocker`
after three failed attempts. There is no `simplify` step: `ce-debug` keeps the
fix minimal.

## Harness and skills prerequisites

The shipped workflows deliberately omit `harness:` on every agent action. A
new attempt uses the daemon's effective default: the durable operator override
when present, otherwise `agent_harness` from configuration, otherwise
`claude-code`. An explicit `harness:` in a user workflow still wins. Inspect or
change the daemon selection in the TUI with `h`, or through
`GET`/`PUT`/`DELETE /settings/agent-harness` on its authenticated local API.

The bundled workflows do not set `ref` for the workspace. A task fetches the
branch of the repository configuration, or the remote's `HEAD` when none is
set. A `ref` in your own workflow wins over that branch.

These are prompts for external skill packages, not bundled implementations:
the compound-engineering workflow needs the referenced `ce-*` skills and the
Open Mercato workflow needs the referenced `om-*` skills available to the
selected harness. Narsor does not install, translate, or migrate those skills;
an operator should install compatible packages before running the workflow.

The compound-engineering workflow works on GitHub and on GitLab. Its `finalize`
step is an agent step: the agent finds the host from `pr_url` and the `origin`
remote, then uses `gh` (GitHub) or `glab` (GitLab). The place where the step
runs needs that tool authenticated, under an identity that may take the pull
request out of Draft. Without that, `finalize` answers `blocked` and the run stops at the
`finalize_blocked` gate with the reason. A person fixes the environment and
chooses `retry`, or chooses `confirm_failure` to end the run in `finalize_failed`.

Protection against a merge after the Draft is gone (for example branch rules)
stays with the repository host.

The agent keeps to the limits written in the prompt (validated `pr_url`, a
closed list of host operations, no `git merge`, no push). The prompt is not a
technical barrier. For this workflow, use an account with minimal permissions.

## Running one

```sh
narsor validate workflows/compound-engineering.yaml

narsor repo add acme-web --remote git@github.com:acme/web.git   # once
narsor run workflows/compound-engineering.yaml \
  --input feature_description="let a reviewer subscribe to a run" \
  --input target_repo=acme-web

narsor show <execution-id>          # where it is waiting
narsor input <execution-id> ...     # answer the gate and resume
```

`target_repo` resolves through the repository registry and falls back to a raw
remote or path. A run stops at every human gate and reports which state it is
waiting in; answer it with the `select` field the gate declares, and the run
continues from there. Every answer becomes a context variable, which is why the
later prompts can quote what you told them.

## Where the human gates are, and why

The gates are not decoration at the end of the pipeline. They sit where a
decision is **irreversible**, **outward-facing**, or **beyond what an agent can
settle from the repository alone**. An autonomous pipeline without them drives
straight to a merge.

### `compound-engineering.yaml` — five stages

Every stage has the same shape: an author step, publication to one Draft merge
request, a review gate where a person decides, and an optional feedback check
after the gate.
The first publication opens the Draft merge request against the repository's
default branch and stores its URL as `pr_url`; every later one pushes to it.

```
1 requirements  brainstorm → doc_review* → publish_requirements → review_requirements → (check_requirements_feedback)
                brainstorm → publish_requirements (the agent skipped the review)
2 plan          plan → plan_review* → publish_plan → review_plan → (check_plan_feedback)
3 implementation work → simplify → code_review → verify_work → publish_work → review_work → (check_work_feedback)
4 learning      compound → publish_learning → review_learning → (check_learning_feedback)
5 finalize      finalize → finished
```

`*` — the review step fixes what it finds and reports `review_summary`. The
result is not held for a gate: the publish step pushes it and adds the
summary to the merge request as a comment, and the person reads it at the gate.

`brainstorm` never asks the person. It writes its assumptions in the section
"Założenia do potwierdzenia" and its questions in "Pytania otwarte dla
użytkownika"; the person answers on the merge request. For a small and obvious
change the agent may skip the requirements review (choice `requirements_ready`).
It then gives the reason in `review_summary`, which the publish step puts on the
merge request, and the reason stays in the task history. `plan_review` always
runs.

| Gate | Choices | Why there |
|---|---|---|
| `review_requirements` | `check_feedback`, `accept`, `revise`, `abandon` | Every later step is measured against the requirements; a wrong scope corrected here costs one conversation. |
| `review_plan` | `check_feedback`, `accept`, `revise`, `revise_requirements`, `abandon` | The last point before code exists. The plan can also show that the requirements are wrong. |
| `review_work` | `check_feedback`, `accept`, `revise`, `revise_plan`, `abandon` | The person reviews the published implementation. |
| `review_learning` | `check_feedback`, `accept`, `revise`, `drop` | The implementation is already accepted, so this gate keeps, revises or drops the learning; it cannot stop the pipeline. |

Every choice that sends a stage back (`revise`, `revise_requirements`,
`revise_plan`) needs a comment and continues the session of the stage's author
with it (`resume_comment_field`): `brainstorm` for the requirements, `plan` for
the plan, `work_revision` for the implementation, `compound` for the learning.
The comment field is optional for `check_feedback` and `accept`. Only the note
from `review_requirements` is carried on: the plan reads it.

`check_feedback` runs the stage's `check_*_feedback` step, which runs
`ce-resolve-pr-feedback` on `pr_url` for the feedback posted while the person
was reading. For requirements and plan it judges the size of a fix:
`fixed_minor` publishes and asks for a review again, `fixed_major` goes back to
the review step. For implementation, `fixed` goes back to `code_review`; for
learning, it publishes again. `needs_human` returns to the gate, which shows
the agent's `reason`. `accept` skips that step and goes at once to the first
step of the next stage (`plan`, `work`, `compound`, `finalize`): the person who
accepts takes responsibility for the comments on the merge request.

Blockers go back to the author of the earlier stage. `plan` chooses
`needs_requirements_revision` with `requirements_blocker`, and `brainstorm`
reads it; `work` chooses `needs_plan_revision` with `plan_blocker`, and `plan`
reads it. The step that reads a blocker also defines it and clears it in its
answer, so an old blocker does not come back on a later pass. `plan` and
`compound` read their own earlier document through `{{ artifacts.plan_doc }}`
and `{{ artifacts.learning_doc }}`, empty on the first pass.

Implementation: `simplify` runs once, after `work`. Every later change of code
— `fix_findings`, `repair_verification`, `work_revision`, a fix from
`check_work_feedback` — goes through `code_review → verify_work → publish_work
→ review_work`. `publish_work` rewrites the description of the merge request on
every push, so it describes the whole change: the code, the requirements and
the plan. The other publish steps leave the description as it is. Review findings reach `fix_findings`
through `{{ steps.code_review.reply }}`. `verify_work` runs `scripts/verify.sh`
from the target repository when that file exists; a missing file skips the
check, and a failure goes to `repair_verification`, which receives
`{{ steps.verify_work.stdout }}` and `{{ steps.verify_work.stderr }}`. During
`ce-work`, the agent can ask the person a question and resume the same step
with the answer.

Learning: `compound` either documents (`documented` → publish) or has nothing
to document and goes straight to `finalize`. `drop` at the gate removes the
document through `drop_learning` and then finalizes.

Finalize is an agent step (`workspace_access: read-only`, `timeout: 10m`): it
takes the pull request out of Draft and does nothing more. It does not approve
it, does not set auto-merge, does not merge it and does not wait for the
checks; a person merges it on the repository host. The number and repository
come from `pr_url` and the pull request, never from a branch name. A pull
request that is already out of Draft or already merged is `ready` too. `ready`
goes to `finished`; `blocked` (no tool, not signed in, a bad URL, a closed pull
request) and `timeout` (the engine stopped the step at its limit) go to the
`finalize_blocked` gate: `retry` runs `finalize` again (no limit), `confirm_failure`
goes to `finalize_failed`. The step reads the pull request from the host after it takes
it out of Draft and tries at most 3 times; `ready` needs a read that shows the
pull request out of Draft or merged.

Loop limits (`max_entries`): `code_review` 10, `repair_verification` 5.
Terminals: `finished` (out of Draft and ready for review, completed),
`cancelled` (`abandon` at a gate) and `finalize_failed` (a person confirmed at the `finalize_blocked` gate that finalize could not confirm
that the pull request is out of Draft, failed). `abandon` does not close the pull request.

Tasks that run on an earlier version of this workflow, with the earlier
gates or the merge steps, may not resume after an update. Finish or cancel them
before you update. A task that waited at an earlier `finalize_blocked` gate with the
`finish_as_draft` choice may not resume.

The graph is drawn in `compound-engineering-flow.html`.

### Optional repository verification script

Add `scripts/verify.sh` to a target repository to opt in to the pre-publication
gate. It runs from the repository root with `CI=true`. Exit `0` only when
formatting, build and tests all pass; any nonzero exit means the code needs
repair. The script may call smaller project scripts internally. For a Rust
repository, its contents could be:

```bash
#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all -- --check
cargo build --workspace
cargo test --workspace
```

Use a formatting check here, since the gate runs after code review. If the
format needs changing, the repair step applies it and the changed code goes
through review and the complete script again. An existing file that is
unreadable or is not a regular file fails the gate; only absence skips it.
Narsor retains up to 64 KiB of each output stream for the repair prompt.

### `open-mercato-sdlc.yaml` — 5 gates

Every one of these is named by `SDLC.md` itself.

| Gate | Sits after | The rule it enforces |
|---|---|---|
| `confirm_readiness` | Intake | The Definition of Ready has a tier *"Ticket-level — only a human can supply these"*: the problem and who has it, the expected outcome and how it is checked, what is out of scope, blocking questions answered, confirmed assumptions. A spec supplies the second tier, never this one. The gate also carries `waived`, because *"a maintainer may waive an item by saying so on the ticket"*. |
| `approve_design` | `om-ux-shape`, on the user-facing path only | *"For a user-facing change, the flow and its states are settled before the code exists."* `assess_surface` routes a change that touches no UI straight to `implement`, so the gate is skipped exactly when the spec says it is. QA later fails the change against this contract, so it is signed off before the code, not discovered in review. |
| `qa_signoff` | `om-auto-qa-pr` attached its evidence | The one hard rule of the process: a `needs-qa` PR must not merge without `qa-approved`. *"Manual means a person judges the result and owns `qa-approved`"*, and *"only a QA reviewer sets the `qa` pipeline label"* — so the automation attaches evidence and touches no labels. The `qa_head` field is the `QA head: <sha>` line that pins the approval to a commit. `qa_failed` routes back to `implement`. |
| `approve_merge` | `om-merge-buddy` reports mergeable | The squash-merge is the point of no return; `om-approve-merge-pr` re-checks every gate before it. `hold` is the human `do-not-merge`. |
| `blocked_gate` | any dead end in the pipeline | *"`blocked` and `do-not-merge` are set and cleared by humans and stop the flow wherever it is."* Every blocked path routes here instead of failing silently, and a person decides whether the work resumes at implementation, at review, or not at all. |

Loops: `code_review → address_feedback → code_review` (the review loop),
`qa_signoff(qa_failed) → implement` (QA failure back to implementation),
`validation_gate(red) → implement`, `confirm_readiness(not_ready) → intake`,
`approve_design(reshape) → shape_design`, and `blocked_gate` back into either
implementation or review.

Terminals: `completed`, `closed_no_action` (triage found nothing to do — a
clean stop, not a failure), `failed` and `cancelled`.

## Notes for anyone editing these files

* **A human gate needs exactly one `select` field whose options are the state's
  `outputs`.** A `text` field carries no output label; a gate that only asks for
  prose stalls the workflow forever. `tests/workflows.rs` asserts this
  structurally for every human state in both files.
* **Every agent prompt must name every label its state declares**, because the
  executor refuses to guess one. That is asserted too, so a prompt and its
  `outputs` cannot drift apart.
* **Descriptive inputs are `type: text`, identifiers stay `type: string`.** The
  TUI shows a `text` input as a large multi-line field and every other input in
  a narrow bar; a workflow without a `text` input gets a bar and nothing else.
  For the CLI and the API both are plain strings.
* **Templates are substitution, not a language.** No filters, no defaults. A
  `{{ context.X }}` or `{{ artifacts.X }}` that some step or gate declares
  renders empty until it is set; a name nobody declares fails the action.
  Declare every variable a later step or `display` reads on the step that sets
  it — an undeclared one never reaches the context.
* `terminal_states` uses the mapping form. The list form would report a failure
  as a success.
* Every agent state carries a timeout; a stalled agent would otherwise hang the
  run for good.
* The built-ins must keep `harness` absent. `tests/builtin_workflows.rs` guards
  that contract against an accidental reintroduction of a Claude-only default.
