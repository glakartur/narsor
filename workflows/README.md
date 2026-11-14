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
  and a loop back to the work for each question or note there.

It needs an agent harness, the `compound-engineering` skills and `gh` or
`glab`. Run it by its path from a checkout of this repository:

```sh
narsor-cli validate workflows/compound-engineering-change.yaml
narsor-cli run workflows/compound-engineering-change.yaml --input description="Add a health endpoint"
```

# Example: observer author

`observer-author.yaml` is an example, not a built-in. An agent writes the
script of an [observer](../docs/user/observers.md) and a description of it,
from the script contract and the example scripts. Then a person accepts the
draft or sends it back to the agent with a comment:

```text
draft -> review -> accepted
  ^        |
  +--------+ revise, with a comment
```

The workflow does not save, try or approve the script, and the task gets no
rights on observers. After `accept`, the owner continues with the CLI:
`observer create`, `observer script set`, `observer dry-run` and
`observer approve`. The repository of the task must be a checkout of Narsor,
or hold a copy of `docs/user/observers.md` and `workflows/observers/examples/`.

```sh
narsor-cli validate workflows/observer-author.yaml
narsor-cli run workflows/observer-author.yaml --input name=jira-new \
  --input source=jira --input goal="A triage task for every new bug in project ABC"
```

# Example observer scripts

[`observers/examples/`](observers/examples/) has three observer scripts. They
use `curl` and `jq`:

- `jira-new-issues.sh` creates a task for each new Jira issue that a JQL
  query finds. On the first run it only records where to start.
- `github-pr-review.sh` and `gitlab-mr-review.sh` deliver `review.approved`,
  `review.changes_requested`, `merged` and `closed` to the tasks that wait
  for a pull or merge request on the watch list.

The header of each script has the `observer create` command, the condition
and the secrets. With `FAKE_SOURCE_DIR` set, a script reads its answers from
files and does not use the network.
