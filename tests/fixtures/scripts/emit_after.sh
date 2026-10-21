#!/bin/sh
# Counts its own invocations in the file named by the first argument and prints
# a §3.3.B JSON payload: the second argument's label on the first invocation,
# the third argument's label on every later one. Used to drive a workflow loop
# out of its cycle on the second pass.
count=0
[ -f "$1" ] && count=$(cat "$1")
count=$((count + 1))
printf '%s' "$count" > "$1"

if [ "$count" -lt 2 ]; then
    printf '{"choice": "%s", "variables": {"passes": %s}}\n' "$2" "$count"
else
    printf '{"choice": "%s", "variables": {"passes": %s}}\n' "$3" "$count"
fi
