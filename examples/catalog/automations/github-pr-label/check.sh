#!/bin/sh
# github-pr-label, phase `check`: answers the gate when the reviewer removed
# the review label from the pull request. See automation.yaml for the
# condition and the params, and docs/user/observers.md for the contract.
#
# One run checks one queue entry (the input's `entry`). The run starts only
# after in.sh put the label on the pull request. When the pull request is
# open and does not carry the label, the script answers the gate:
#
#   narsor-cli task decide TASK --episode EPISODE --choice CHOICE \
#     --reason "the label LABEL was removed from MR_URL"
#
# Exit code (the script prints nothing on stdout):
#   0  the run is done: the script answered the gate, or it has nothing to
#      answer in this run (the label is still there, the pull request is not
#      open, or mr_url is not a pull request URL on github.host).
#   2  an error (a bad input, condition or param, no token, a source that
#      does not answer or refuses the token, a rate limit, a refused
#      answer). Narsor records it for the entry and checks the entry again
#      in the next run.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the pull request from
# "$FAKE_SOURCE_DIR/pr-OWNER-REPO-NUMBER.json" and does not use the network.
# A file pr-OWNER-REPO-NUMBER.status next to it holds the HTTP status to act
# as if the source gave (for example 404).

set -eu

me=github-pr-label/check
phase=check

DONE=0
ERROR=2

fail() {
    printf '%s: %s\n' "$me" "$*" >&2
    exit "$ERROR"
}

done_with() {
    printf '%s: %s\n' "$me" "$*" >&2
    exit "$DONE"
}

command -v jq >/dev/null 2>&1 || fail "jq is not installed"

input=$(cat)
printf '%s' "$input" | jq -e --arg phase "$phase" '.contract_version == 3 and .phase == $phase' >/dev/null ||
    fail "the input is not version 3 of the script contract for the phase $phase"
entry=$(printf '%s' "$input" | jq -c '.entry // empty')
[ -n "$entry" ] || fail "the input has no queue entry: the queue must have mode on_episodes"

host=$(printf '%s' "$input" | jq -r '.condition.github.host // "github.com"')
api_url=$(printf '%s' "$input" | jq -r '.condition.github.api_url // "https://api.github.com" | sub("/+$"; "")')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.github.host must be a host name"
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.github.api_url must be an https:// address without a user name, query or fragment"
label=$(printf '%s' "$input" | jq -r '.condition.label // "narsor-review"')
[ -n "$label" ] || fail "condition.label must be a label name"

param() {
    printf '%s' "$entry" | jq -r --arg name "$1" '.params[$name] // ""'
}

mr_url=$(param mr_url)

# Not our entry: check mr_url before the token goes anywhere.
pr=$(jq -n -c --arg url "$mr_url" --arg host "$host" '
    [$url | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<owner>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/(?<repo>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/pull/(?<number>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$pr" ] || done_with "'$mr_url' is not a pull request URL on $host; not mine, not fetched"

task_id=$(printf '%s' "$entry" | jq -r '.task_id')
episode=$(printf '%s' "$entry" | jq -r '.episode')
choice=$(param choice)
[ -n "$choice" ] || choice=check_feedback
printf '%s' "$entry" | jq -e --arg choice "$choice" 'any(.choices[]; .name == $choice)' >/dev/null ||
    fail "the gate $(printf '%s' "$entry" | jq -r '.step') has no choice $choice"

owner=$(printf '%s' "$pr" | jq -r '.owner')
repo=$(printf '%s' "$pr" | jq -r '.repo')

# Optional allowlist: the token never reaches a repository that is not on it.
allowed=$(printf "%s" "$input" | jq -c ".condition.github.repos // null")
if [ "$allowed" != null ]; then
    printf "%s" "$allowed" | jq -e "type == \"array\" and all(.[]; type == \"string\")" >/dev/null ||
        fail "condition.github.repos must be a list of owner/repo names"
    printf "%s" "$allowed" | jq -e --arg r "$owner/$repo" "any(.[]; ascii_downcase == (\$r | ascii_downcase))" >/dev/null ||
        done_with "$owner/$repo is not in condition.github.repos; not mine, not fetched"
fi

number=$(printf '%s' "$pr" | jq -r '.number')
name="$owner-$repo-$number"

if [ -z "${FAKE_SOURCE_DIR:-}" ]; then
    [ -n "${GITHUB_TOKEN:-}" ] || fail "GITHUB_TOKEN is not set: put the secret github-token in the secrets directory"
fi

quote_config() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

# fetch NAME PATH: the JSON answer of GET API_URL/PATH, or an error. The
# token goes to curl on stdin (--config -), never on its command line.
fetch() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        code=200
        [ ! -f "$FAKE_SOURCE_DIR/$1.status" ] || code=$(cat "$FAKE_SOURCE_DIR/$1.status")
        body=$(cat "$FAKE_SOURCE_DIR/$1.json" 2>/dev/null || true)
    else
        response=$(
            printf 'header = "Authorization: Bearer %s"\n' "$(quote_config "$GITHUB_TOKEN")" |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --header 'Accept: application/vnd.github+json' \
                    --header 'X-GitHub-Api-Version: 2022-11-28' \
                    --write-out '\n%{http_code}' \
                    "$api_url/$2"
        ) || fail "cannot reach $api_url"
        code=$(printf '%s\n' "$response" | tail -n 1)
        body=$(printf '%s\n' "$response" | sed '$d')
    fi
    case $code in
    2??) printf '%s\n' "$body" ;;
    *) fail "cannot read $mr_url: HTTP $code (gone, a rate limit, or the token may not read it)" ;;
    esac
}

answer=$(fetch "pr-$name" "repos/$owner/$repo/pulls/$number") || exit "$ERROR"
printf '%s' "$answer" | jq -e 'type == "object" and (.labels | type) == "array"' >/dev/null ||
    fail "the answer for $mr_url is not a pull request"

state=$(printf '%s' "$answer" | jq -r 'if .merged == true or .merged_at != null then "merged" else .state end')
[ "$state" = open ] || done_with "$mr_url is $state; nothing to answer"
# `$wanted`, not `$label`: jq 1.6 reserves `label` as a keyword. A jq error
# fails the run instead of reading as "no label".
has_label=$(printf '%s' "$answer" | jq -r --arg wanted "$label" 'any(.labels[]; (.name | ascii_downcase) == ($wanted | ascii_downcase))') ||
    fail "cannot read the labels of $mr_url"
if [ "$has_label" = true ]; then
    done_with "$mr_url still has the label $label"
fi

narsor-cli task decide "$task_id" --episode "$episode" --choice "$choice" \
    --reason "the label $label was removed from $mr_url" </dev/null >&2 ||
    fail "the answer $choice for $mr_url was refused"
exit "$DONE"
