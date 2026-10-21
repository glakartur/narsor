#!/bin/sh
# A stand-in for the GitLab CLI (`glab`).
#
# `finalize` and `disarm_merge` of `workflows/compound-engineering.yaml` merge
# a merge request through `glab`. The real one needs a GitLab server and
# credentials, and a merge cannot be undone, so the test suite never runs it.
# The tests install this script as `glab` in a directory of its own, put that
# directory first on the step's `PATH`, and drive it with files there:
#
#   view.<n>, view   the merge request JSON that the n-th `glab mr view`
#                    prints (per call, then the fallback). A `--jq` filter is
#                    applied with the real `jq`, as `glab` applies its own.
#   hang.<command>   seconds to sleep before `glab mr <command>` (or
#                    `glab <command>`) answers — for step timeout tests.
#   fail.<command>   text printed on stderr; the command then exits 1.
#
# and which it writes back:
#
#   calls            one line per call, with its arguments
set -u

dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/calls"

command=${1-}
if [ "$command" = mr ]; then
    command=${2-}
fi

if [ -f "$dir/hang.$command" ]; then
    sleep "$(cat "$dir/hang.$command")"
fi

if [ -f "$dir/fail.$command" ]; then
    cat "$dir/fail.$command" >&2
    exit 1
fi

if [ "$command" = view ]; then
    count=0
    if [ -f "$dir/view_count" ]; then
        count=$(cat "$dir/view_count")
    fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$dir/view_count"

    view="$dir/view"
    if [ -f "$dir/view.$count" ]; then
        view="$dir/view.$count"
    fi

    filter=
    previous=
    for arg in "$@"; do
        if [ "$previous" = --jq ]; then
            filter=$arg
        fi
        previous=$arg
    done

    if [ -n "$filter" ]; then
        jq -r "$filter" "$view"
    else
        cat "$view"
    fi
fi

exit 0
