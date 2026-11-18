#!/bin/sh
# gitlab-mr-label, phase `out`: removes the review label from the merge
# request when the gate exits. See automation.yaml for the condition and the
# params, and docs/user/observers.md for the contract.
#
# Idempotent: when the label is already gone (the reviewer removed it, which
# is how check.sh answered the gate), the script changes nothing. The input's
# `exit` tells how the gate was left; the script logs it and removes the
# label in every case, also when the task ended at the gate.
#
# Exit code (the script prints nothing on stdout):
#   0  the merge request has no label, or mr_url is not a merge request URL
#      on gitlab.host (not this automation's entry: nothing is sent).
#   2  an error. Narsor retries the script (after 10 s, then 30 s); after the
#      third failure it records the failure in the task history and the gate
#      exits all the same.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the merge request from
# "$FAKE_SOURCE_DIR/mr-NAME.json" (NAME as in check.sh) and appends each
# change it would send to "$FAKE_SOURCE_DIR/writes.log" as one line
# "METHOD PATH FIELD=VALUE"; a file writes.status holds the HTTP status to
# act as if the change got. It does not use the network.

set -eu

me=gitlab-mr-label/out
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

host=$(printf '%s' "$input" | jq -r '.condition.gitlab.host // ""')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.gitlab.host must be a host name"
api_url=$(printf '%s' "$input" | jq -r --arg host "$host" '
    .condition.gitlab.api_url // ("https://" + $host + "/api/v4") | sub("/+$"; "")')
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.gitlab.api_url must be an https:// address without a user name, query or fragment"
label=$(printf '%s' "$input" | jq -r '.condition.label // "narsor-review"')
case $label in
'' | *,*) fail "condition.label must be a label name without a comma" ;;
esac

mr_url=$(printf '%s' "$entry" | jq -r '.params.mr_url // ""')

# Not our entry: check mr_url before the token goes anywhere.
mr=$(jq -n -c --arg url "$mr_url" --arg host "$host" '
    [$url | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<project>[A-Za-z0-9_][A-Za-z0-9_.-]*(/[A-Za-z0-9_][A-Za-z0-9_.-]*)+)/-/merge_requests/(?<iid>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$mr" ] || done_with "'$mr_url' is not a merge request URL on $host; not mine, not fetched"

project=$(printf '%s' "$mr" | jq -r '.project')
iid=$(printf '%s' "$mr" | jq -r '.iid')
encoded=$(printf '%s' "$project" | jq -R -r '@uri')
name=$(printf '%s-%s' "$project" "$iid" | tr '/' '-')
path="projects/$encoded/merge_requests/$iid"

if [ -z "${FAKE_SOURCE_DIR:-}" ]; then
    [ -n "${GITLAB_TOKEN:-}" ] || fail "GITLAB_TOKEN is not set: put the secret gitlab-token in the secrets directory"
fi

quote_config() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

# request METHOD PATH [FIELD=VALUE]: the JSON answer of METHOD API_URL/PATH,
# or an error. The token and the field go to curl on stdin (--config -),
# never on its command line.
request() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        code=200
        if [ "$1" = GET ]; then
            [ ! -f "$FAKE_SOURCE_DIR/mr-$name.status" ] || code=$(cat "$FAKE_SOURCE_DIR/mr-$name.status")
            body=$(cat "$FAKE_SOURCE_DIR/mr-$name.json" 2>/dev/null || true)
        else
            printf '%s %s %s\n' "$1" "$2" "${3:-}" >>"$FAKE_SOURCE_DIR/writes.log"
            [ ! -f "$FAKE_SOURCE_DIR/writes.status" ] || code=$(cat "$FAKE_SOURCE_DIR/writes.status")
            body='{}'
        fi
    else
        response=$(
            {
                printf 'header = "PRIVATE-TOKEN: %s"\n' "$(quote_config "$GITLAB_TOKEN")"
                [ -z "${3:-}" ] || printf 'data-urlencode = "%s"\n' "$(quote_config "$3")"
            } |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --request "$1" \
                    --header 'Accept: application/json' \
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

answer=$(request GET "$path") || exit "$ERROR"
printf '%s' "$answer" | jq -e 'type == "object" and (.labels | type) == "array"' >/dev/null ||
    fail "the answer for $mr_url is not a merge request"
if ! printf '%s' "$answer" | jq -e --arg label "$label" 'any(.labels[]; . == $label)' >/dev/null; then
    done_with "$mr_url has no label $label"
fi

request PUT "$path" "remove_labels=$label" >/dev/null || exit "$ERROR"
done_with "the label $label is removed from $mr_url"
