#!/bin/sh
# Announces the greeting it was handed and reports it back as a context
# variable, so that a CLI smoke test can see a script result reach `show`.
printf '{"output": "success", "variables": {"announced": "%s"}}\n' "$1"
exit 0
