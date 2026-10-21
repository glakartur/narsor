#!/bin/sh
# Stands in for the agent that applies a gate's answer: hands the answer it was
# given back as a context variable, so a test can see that the text the
# operator typed at the gate reached the next step.
printf '{"choice": "applied", "variables": {"applied_answer": "%s"}}\n' "$1"
exit 0
