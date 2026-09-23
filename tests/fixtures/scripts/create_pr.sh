#!/bin/sh
# Stub for the `create_pr` state of the WORKFLOW_SCHEMA.md §4 example workflow.
# The state declares only the label `created`, which no exit code maps to, so
# the label is reported through the §3.3.B JSON payload on stdout — together
# with a variable and an artifact, to exercise both engine paths.
printf '{"output": "created", "variables": {"pr_number": 123}, "artifacts": [{"name": "pull_request", "type": "text/url", "path": "https://example.invalid/pr/123"}]}\n'
