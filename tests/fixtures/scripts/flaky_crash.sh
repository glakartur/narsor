#!/bin/sh
# Counts its own invocations in the file named by the first argument and dies by
# signal — an action error, not a mapped exit code — for the first N of them
# (N = the second argument). Later invocations succeed with the label `success`.
count=0
[ -f "$1" ] && count=$(cat "$1")
count=$((count + 1))
printf '%s' "$count" > "$1"

if [ "$count" -le "$2" ]; then
    printf 'attempt %s fails\n' "$count" >&2
    kill -9 $$
fi

printf '{"output": "success", "variables": {"attempts": %s}}\n' "$count"
