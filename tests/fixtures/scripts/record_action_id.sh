#!/bin/sh
# Appends the idempotency key the orchestrator passed in the environment to the
# file named by the first argument, twice per invocation, so a test can see both
# that the key is stable within one attempt and that it changes between retries.
# The first N invocations (N = the second argument) then die by signal, which
# makes the orchestrator retry them.
printf '%s\n' "$ORCHESTRATOR_ACTION_ID" >> "$1"
printf '%s\n' "$ORCHESTRATOR_ACTION_ID" >> "$1"

count=0
[ -f "$2" ] && count=$(cat "$2")
count=$((count + 1))
printf '%s' "$count" > "$2"

if [ "$count" -le "$3" ]; then
    printf 'attempt %s fails\n' "$count" >&2
    kill -9 $$
fi

printf '{"output": "success"}\n'
