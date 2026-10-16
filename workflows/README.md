# Shipped workflows

Two runnable pipelines, written against `WORKFLOW_SCHEMA.md` and modelled on
real agentic development processes rather than invented ones:

| File | Process | Source material |
|---|---|---|
| `compound-engineering.yaml` | The compound engineering chain: brainstorm → plan → work → code review → push/PR → human review → compound → push → final review | the `ce-*` skills of the `compound-engineering` plugin |
| `open-mercato-sdlc.yaml` | The Open Mercato ticket lifecycle: Discovery → Intake → Triage → Claim → Design → Implement → PR → review loop → QA → Merge → housekeeping | [`SDLC.md`](https://github.com/open-mercato/skills) and the `om-*` skills |

Both validate with zero violations and both are exercised end to end in
`tests/workflows.rs` against a fake harness — no real `claude`, `git` or `gh` is
ever run by the tests.

## Harness and skills prerequisites

The shipped workflows deliberately omit `harness:` on every agent action. A
new attempt uses the daemon's effective default: the durable operator override
when present, otherwise `agent_harness` from configuration, otherwise
`claude-code`. An explicit `harness:` in a user workflow still wins. Inspect or
change the daemon selection in the TUI with `h`, or through
`GET`/`PUT`/`DELETE /settings/agent-harness` on its authenticated local API.

These are prompts for external skill packages, not bundled implementations:
the compound-engineering workflow needs the referenced `ce-*` skills and the
Open Mercato workflow needs the referenced `om-*` skills available to the
selected harness. Narsor does not install, translate, or migrate those skills;
an operator should install compatible packages before running the workflow.

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

### `compound-engineering.yaml` — 5 gates

| Gate | Sits after | Why there |
|---|---|---|
| `clarify_direction` | `ce-brainstorm` returned `needs_direction` | Product direction is the one thing an agent has no basis to choose. Rather than guess, the chain stops and asks. |
| `approve_scope` | `ce-brainstorm` + `ce-doc-review` | The requirements document is what every later step is measured against. A wrong scope caught here costs one conversation; caught after the PR it costs the whole pipeline. An optional note is carried into the plan as `{{ context.scope_notes }}`; `revise` sends `brainstorm_feedback` back to the brainstorm. |
| `approve_plan` | `ce-plan`, **before a line of code exists** | The cheapest correction point in the whole process. Everything after this gate is an edit to the repository. |
| `implementation_review` | implementation is pushed to the PR | The person reviews the implementation and can request changes, approve it for learning capture, or stop. |
| `learning_review` | learning has been pushed to the already reviewed PR | The person reviews the updated PR and gives final approval. Requests for changes return to implementation. |

`ce-compound` runs after the implementation review, when the result and review
feedback are known. If it writes a learning document, the workflow pushes that
update to the PR and asks for a final human review. If there is nothing to
document, the approved PR is complete.

Loops: `approve_scope → brainstorm`, `approve_plan → plan`,
`code_review → fix_review_findings → code_review` until clean,
`code_review → publish_work → implementation_review → compound →
publish_learning → learning_review`. A request from either review returns to
`work_revision → simplify → code_review`; `publish_work` then updates the same
PR. During `ce-work`, the agent can also pause its own action with
`awaiting_input`; the answer resumes that same step.

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
* **Templates are substitution, not a language.** Only `{{ inputs.X }}` and
  `{{ context.X }}`; no filters, no defaults. A `{{ context.X }}` that is not
  set when the state is entered fails the action, so a variable is only quoted
  on a path where the gate that sets it has certainly run.
* `terminal_states` uses the mapping form. The list form would report a failure
  as a success.
* Every agent state carries a timeout; a stalled agent would otherwise hang the
  run for good.
* The built-ins must keep `harness` absent. `tests/builtin_workflows.rs` guards
  that contract against an accidental reintroduction of a Claude-only default.
