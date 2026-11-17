# Built-in workflow

`template.yaml` is the only workflow definition shipped by Narsor. It is a safe,
runnable tour of the workflow engine rather than a project automation pipeline:

- a typed input;
- a no-op `printf` script step with one `exit`, returning a variable and
  artifact-path metadata;
- a human `decision` step with an optional comment, whose choices write the
  `outcome` variable with `set`;
- the unified `schema: v2` shape, terminal statuses, a timeout and retry policy.

The script writes no files and calls no external services. Its artifact path is
illustrative metadata only; no file is created. The workflow does not invoke an
agent, so it runs without an agent harness.

Validate and run it with:

```sh
narsor-cli validate template
narsor-cli run template --input example_text="Hello, engine"
```

The run pauses at `review`. Choose `finish` or `cancel`, optionally adding a
comment. Definitions for actual projects belong in the user's workflow
directory or the repository's own `workflows/` directory; they are not shipped
as built-ins.

# Example: compound engineering

`compound-engineering-change.yaml` is an example, not a built-in: Narsor does
not embed it, and an installed binary does not know it. It shows a real
pipeline on one repository. The diagram of its flow, its gates and its loops
is in [`compound-engineering-change.md`](compound-engineering-change.md).
The workflow has:

- brainstorm, plan, work, code review and compound, each run by an agent with
  the `compound-engineering` skills;
- a `profile` step before the work, which chooses the model and the effort of
  the later agent steps within the `ceiling`;
- one Draft pull request, opened after the brainstorm and updated after the
  plan, the code review and the compound; the last step takes it out of Draft;
- human gates that open only when needed: after the brainstorm and the plan
  only when the agent asked something, and after the code review always, to
  approve the whole code;
- the whole conversation with the person in open threads of the pull request,
  and a loop back to the work for each question or note there;
- a `watch` block on each review gate (queue `mr-review`), so that an
  observer can answer the gate when the person removes the label
  `narsor-review` from the pull request.

It needs an agent harness, the `compound-engineering` skills and `gh` or
`glab`. Run it by its path from a checkout of this repository:

```sh
narsor-cli validate workflows/compound-engineering-change.yaml
narsor-cli run workflows/compound-engineering-change.yaml --input description="Add a health endpoint"
```

# Example observer scripts

[`observers/examples/`](observers/examples/) has three observer scripts. They
use `curl` and `jq`:

- `jira-new-issues.sh` creates a task for each new Jira issue that a JQL
  query finds. On the first run it only records where to start.
- `github-pr-review.sh` and `gitlab-mr-review.sh` serve a queue of review
  gates (a `watch` block on a human `decision`, mode `on_episodes`). When the
  reviewer removes the gate's `label` from the pull or merge request in
  `mr_url`, the script answers the gate with `narsor-cli task decide` and the
  gate's `choice` param (default `check_feedback`). Both can serve one
  queue: each one exits 0 with no answer for a URL that is not on its host.

The header of each script has the `observer create` command, the condition,
the params and the secrets. With `FAKE_SOURCE_DIR` set, a script reads its
answers from files and does not use the network.
