#!/bin/sh
# A stand-in for an AI harness CLI (`claude`, `opencode`).
#
# The real harnesses cost money, need credentials and answer differently every
# time, so the test suite never runs them. This script is pointed at by
# `ClaudeCodeAdapter::with_program` / `OpenCodeAdapter::with_program` and is
# driven entirely by files in its own directory, which the test writes:
#
#   stdout, stdout.<n>   what to print on stdout (per call, then the fallback)
#   stderr, stderr.<n>   what to print on stderr (per call, then the fallback)
#   exit, exit.<n>       the exit code (per call, then the fallback; default 0)
#   sleep, sleep.<n>     seconds to sleep after answering (per call, then the
#                        fallback; for timeout tests)
#
# and which it writes back, one set per call:
#
#   count                how many times it has been called
#   pid.<n>              its process id
#   cwd.<n>              its working directory
#   action_id.<n>        the value of ORCHESTRATOR_ACTION_ID
#   args.<n>             its arguments, each terminated by a --%ARG%-- line
set -u

dir=$(dirname "$0")

count=0
if [ -f "$dir/count" ]; then
    count=$(cat "$dir/count")
fi
count=$((count + 1))
printf '%s\n' "$count" > "$dir/count"

printf '%s\n' "$$" > "$dir/pid.$count"
pwd > "$dir/cwd.$count"
printf '%s\n' "${ORCHESTRATOR_ACTION_ID-}" > "$dir/action_id.$count"

: > "$dir/args.$count"
for arg in "$@"; do
    printf '%s\n--%%ARG%%--\n' "$arg" >> "$dir/args.$count"
done

# Output is printed *before* the sleep, and flushed line-by-line (`cat` does
# not buffer), so a streaming reader on the other end of the pipe really does
# see each line arrive before the process is made to hang past a timeout —
# the whole point of the "two lines, then sleep past the timeout" tests.
if [ -f "$dir/stdout.$count" ]; then
    cat "$dir/stdout.$count"
elif [ -f "$dir/stdout" ]; then
    cat "$dir/stdout"
fi

if [ -f "$dir/stderr.$count" ]; then
    cat "$dir/stderr.$count" >&2
elif [ -f "$dir/stderr" ]; then
    cat "$dir/stderr" >&2
fi

if [ -f "$dir/sleep.$count" ]; then
    sleep "$(cat "$dir/sleep.$count")"
elif [ -f "$dir/sleep" ]; then
    sleep "$(cat "$dir/sleep")"
fi

if [ -f "$dir/exit.$count" ]; then
    exit "$(cat "$dir/exit.$count")"
elif [ -f "$dir/exit" ]; then
    exit "$(cat "$dir/exit")"
fi

exit 0
