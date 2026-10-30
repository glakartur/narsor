# Built-in workflow

`template.yaml` is the only workflow definition shipped by Narsor. It is a safe,
runnable tour of the workflow engine rather than a project automation pipeline:

- a typed input and a display link;
- a no-op `printf` script returning a choice, a variable and artifact-path
  metadata;
- a human form with a choice and optional comment;
- deterministic transitions, terminal statuses, a timeout and retry policy.

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
