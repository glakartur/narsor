#!/bin/sh
# github-pr-label, phase `out`: removes the review label from the pull
# request when the gate exits. See automation.yaml for the condition and the
# params, and docs/user/observers.md for the contract.
#
# Idempotent: when the label is already gone (the reviewer removed it, which
# is how check.sh answered the gate), the script changes nothing. The input's
# `exit` tells how the gate was left; the script logs it and removes the
# label in every case, also when the task ended at the gate.
#
# Exit code (the script prints nothing on stdout):
#   0  the pull request has no label, or mr_url is not a pull request URL on
#      github.host (not this automation's entry: nothing is sent).
#   2  an error. Narsor retries the script (after 10 s, then 30 s); after the
#      third failure it records the failure in the task history and the gate
#      exits all the same.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the pull request from
# "$FAKE_SOURCE_DIR/pr-OWNER-REPO-NUMBER.json" and appends each change it
# would send to "$FAKE_SOURCE_DIR/writes.log" as one line
# "METHOD PATH BODY"; a file writes.status holds the HTTP status to act as
# if the change got. It does not use the network.

set -eu

me=github-pr-label/out
phase=out

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
[ -n "$entry" ] || fail "the input has no queue entry"
printf '%s: the gate exited: %s\n' "$me" \
    "$(printf '%s' "$input" | jq -r '.exit | "\(.choice // "no choice") by \(.author)"')" >&2

host=$(printf '%s' "$input" | jq -r '.condition.github.host // "github.com"')
api_url=$(printf '%s' "$input" | jq -r '.condition.github.api_url // "https://api.github.com" | sub("/+$"; "")')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.github.host must be a host name"
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.github.api_url must be an https:// address without a user name, query or fragment"
label=$(printf '%s' "$input" | jq -r '.condition.label // "narsor-review"')
[ -n "$label" ] || fail "condition.label must be a label name"

mr_url=$(printf '%s' "$entry" | jq -r '.params.mr_url // ""')

# Not our entry: check mr_url before the token goes anywhere.
pr=$(jq -n -c --arg url "$mr_url" --arg host "$host" '
    [$url | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<owner>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/(?<repo>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/pull/(?<number>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$pr" ] || done_with "'$mr_url' is not a pull request URL on $host; not mine, not fetched"

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

# request METHOD PATH [BODY]: the JSON answer of METHOD API_URL/PATH, or an
# error. The token and the body go to curl on stdin (--config -), never on
# its command line.
request() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        code=200
        if [ "$1" = GET ]; then
            [ ! -f "$FAKE_SOURCE_DIR/pr-$name.status" ] || code=$(cat "$FAKE_SOURCE_DIR/pr-$name.status")
            body=$(cat "$FAKE_SOURCE_DIR/pr-$name.json" 2>/dev/null || true)
        else
            printf '%s %s %s\n' "$1" "$2" "${3:-}" >>"$FAKE_SOURCE_DIR/writes.log"
            [ ! -f "$FAKE_SOURCE_DIR/writes.status" ] || code=$(cat "$FAKE_SOURCE_DIR/writes.status")
            body='{}'
        fi
    else
        response=$(
            {
                printf 'header = "Authorization: Bearer %s"\n' "$(quote_config "$GITHUB_TOKEN")"
                [ -z "${3:-}" ] || printf 'data-raw = "%s"\n' "$(quote_config "$3")"
            } |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --request "$1" \
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
    *) fail "$1 $mr_url: HTTP $code (gone, a rate limit, or the token may not change it)" ;;
    esac
}

answer=$(request GET "repos/$owner/$repo/pulls/$number") || exit "$ERROR"
printf '%s' "$answer" | jq -e 'type == "object" and (.labels | type) == "array"' >/dev/null ||
    fail "the answer for $mr_url is not a pull request"
if ! printf '%s' "$answer" | jq -e --arg label "$label" 'any(.labels[]; (.name | ascii_downcase) == ($label | ascii_downcase))' >/dev/null; then
    done_with "$mr_url has no label $label"
fi

encoded=$(printf '%s' "$label" | jq -R -r '@uri')
request DELETE "repos/$owner/$repo/issues/$number/labels/$encoded" >/dev/null || exit "$ERROR"
done_with "the label $label is removed from $mr_url"
