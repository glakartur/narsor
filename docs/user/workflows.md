# Workflows

The YAML contract in brief and the workflows that ship with Narsor. The full normative schema is in [the workflow schema](../dev/workflow-schema.md).

## The YAML contract in brief

The full contract is [`workflow-schema.md`](../dev/workflow-schema.md). Two points about `terminal_states`,
because it accepts two forms:

```yaml
# Mapping form — every terminal state names the status it produces.
# Allowed values: completed, failed, cancelled.
terminal_states:
  completed: completed
  failed: failed
  abandoned: cancelled

# List form — the older one. Every entry means `completed`.
terminal_states:
  - completed
  - failed
```

The difference matters: under the list form a state called `failed` still
finishes the execution as `completed`, because the list declares only *which*
states are terminal and not what they mean. Reach for the mapping form when a
state is supposed to end the run as a failure or a cancellation.

## Workflows: built in, by name, overridable

The `template` workflow is compiled into `narsor-cli`; install the workspace
process binaries as shown under [Building and testing](../dev/building-and-testing.md#building-and-testing)
to run it from anywhere:

```bash
narsor-cli run template --input example_text="Hello, engine"
```

`run`, `validate` and `--file` take a *workflow reference*: a path, or a name
with or without a `.yaml` / `.yml` suffix. It is resolved in this order, first
hit wins:

| # | Where | Source shown as |
|---|-------|-----------------|
| 1 | an existing file at that exact path | `file` |
| 2 | `{root}/workflows/<name>.yaml` (then `.yml`) | `root` |
| 3 | `$XDG_CONFIG_HOME/narsor/workflows/<name>.yaml`, else `$HOME/.config/narsor/workflows/<name>.yaml` | `user` |
| 4 | the workflow built into the binary | `built-in` |

A path always wins, so `narsor-cli run ./draft.yaml` keeps meaning exactly
that file. A copy in one of the directories **overrides** the built-in of the
same name. When nothing matches, the error lists every place that was tried and
names the built-ins.

### Looking at what is reachable

```bash
narsor workflow list          # every workflow, its source, its description
narsor workflow show NAME     # the YAML that would run (stdout); the source on stderr
```

`list` prints one row per name and marks an override rather than listing the
same name twice:

```
NAME                         SOURCE     DESCRIPTION
template                     user       A safe, runnable tour of Narsor workflow features.
                                        overrides: built-in
                                        path: /home/you/.config/narsor/workflows/template.yaml
```

### Making a built-in your own

```bash
narsor workflow eject template        # → the user directory
narsor workflow eject template --to ./workflows
```

`eject` writes the built-in out as a file, creating the directory if needed, and
refuses to overwrite an existing one unless you pass `--force`. The copy takes
precedence from that moment on — edit it and every later `run` of that name uses
your version.

### Starting one from the interface

`narsor`, then `n`: the catalogue opens as a list, each row naming its
source (`built-in`, `user`, `root`, and `overrides: …` where a copy shadows a
built-in). `j` / `k` change the selection and the inputs of the highlighted
workflow are described beside it. `Enter` on a workflow goes into its inputs and
the list folds to a strip with the workflow's name: the short inputs (`string`,
`integer`, `boolean`, `repository`) make a bar, and each `text` input is a
multi-line field — the one with the focus takes the rest of the screen, the
others fold to their first lines. `Enter` queues the task, `Ctrl+J` starts a new
line, `Esc` or `Shift+Tab` from the first field returns to the list with what
was typed kept. A workflow without a `text` input gets the bar and nothing else. The last row, *Enter
a path…*, takes an arbitrary path for a definition you are writing (a local
daemon only; the path is validated first and every violation is listed).

### Resuming what a built-in started

An execution records both the definition's path (when it came from a file) and
its workflow name, in `metadata.cli.definition_path` and
`metadata.cli.definition_name`. A built-in has no path, so the name is what makes
`narsor-cli input` and `narsor-cli resume` — and the interface — find the
definition again in a later process. The path is used first when it is still
there, so executions created before this existed keep resolving unchanged;
`--file <WORKFLOW>` overrides both and accepts a name as well as a path.

---

## Built-in workflow template

`workflows/template.yaml` is the only workflow shipped in the binary. It
demonstrates typed inputs, task display links, script output contracts,
variables, artifact-path metadata, a human choice, terminal statuses, timeouts
and retry policy. Its `printf` step has no side effects; the artifact path is a
placeholder and no file is created. The workflow does not invoke an agent or
external service.

```sh
narsor-cli validate template
narsor-cli run template --input example_text="Hello, engine"
narsor-cli show <execution-id>
narsor-cli input <execution-id> --field decision=finish
```

The run pauses at its human gate. Choose `finish` or `cancel`; project-specific
workflow definitions can live in the project's or user's workflow directory.
[`workflows/README.md`](../../workflows/README.md) describes the example.
