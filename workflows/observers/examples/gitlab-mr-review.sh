#!/bin/sh
# Narsor observer example: events for tasks that wait for the review of a
# GitLab merge request. Read docs/user/observers.md first.
#
# Create the observer once, as the owner. It looks only at the merge
# requests on its watch list; a `wait` step with a `watch` block puts one
# there while the task waits (docs/dev/workflow-schema.md, WaitAction):
#
#   narsor-cli observer create mr-review --source gitlab.example.com \
#     --deliver-events --allow-prefix https://gitlab.example.com/ \
#     --secret gitlab-token=GITLAB_TOKEN \
#     --condition '{"host": "gitlab.example.com"}'
#   narsor-cli observer script set mr-review --file gitlab-mr-review.sh
#   narsor-cli observer approve mr-review --version 1
#
# Condition (a JSON object, given to the script as it is):
#   host     the host of the merge request URLs. Required.
#   api_url  the https:// address of the REST API. Default:
#            https://HOST/api/v4.
#
# Secrets: GITLAB_TOKEN, a token with the read_api scope.
#
# One run checks one watch entry (the input's `watch`). Its object is a
# merge request URL:
#   https://HOST/GROUP/PROJECT/-/merge_requests/IID
# (subgroups allowed). Before it uses the token, the script checks the
# object: an object that is not a merge request URL on `host` is never
# fetched, so the token is not sent for it (fail closed). The control plane
# checks the objects too (--allow-prefix); this check is the second line of
# defence.
#
# Events (the entry gets only the types in its event_types):
#   review.approved            the merge request has its required approvals
#   review.changes_requested   a reviewer asked for changes
#   merged                     the merge request is merged
#   closed                     the merge request is closed without a merge
# Event data: {"mr_url": URL, "iid": N}. Filter a `wait` step on mr_url.
#
# Exit code (the script prints nothing on stdout):
#   0  done: Narsor removes the entry. A review event went to the waiting
#      step of the task; or the merge request is merged or closed; or the
#      object is not a merge request URL on `host`, or the merge request is
#      gone or the token may not read it (HTTP 404, 403 or 410).
#   1  check the entry again in the next run: no review decision, the task
#      does not wait (no wait_episode), or the task does not wait for this
#      event (narsor-cli says "recorded", not "delivered").
#   2  an error (a bad input or condition, no token, a source that does not
#      answer, a rate limit, a refused event). Narsor records it and checks
#      the entry again in the next run.
#
# When a task waits again for the same merge request (a review loop), its
# `wait` step puts the merge request on the watch list again, and the old
# approvals and reviewer states on GitLab still match. Mark the merge
# request before it goes back on the list (for example with a label that
# the script looks for); this example does not do that.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answers from
# "$FAKE_SOURCE_DIR/mr-NAME.json", "$FAKE_SOURCE_DIR/approvals-NAME.json"
# and "$FAKE_SOURCE_DIR/reviewers-NAME.json", where NAME is
# GROUP-PROJECT-IID (every / of the project path a -), and does not use the
# network. A file NAME.status next to an answer holds the HTTP status to
# act as if the source gave (for example 404).

set -eu

me=gitlab-mr-review

# The exit codes of the contract.
DONE=0
AGAIN=1
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
printf '%s' "$input" | jq -e '.contract_version == 1' >/dev/null ||
    fail "the input is not version 1 of the script contract"
entry=$(printf '%s' "$input" | jq -c '.watch // empty')
[ -n "$entry" ] || fail "the input has no watch entry: create the observer with --deliver-events"

host=$(printf '%s' "$input" | jq -r '.condition.host // ""')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.host must be a host name"
api_url=$(printf '%s' "$input" | jq -r --arg host "$host" '
    .condition.api_url // ("https://" + $host + "/api/v4") | sub("/+$"; "")')
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.api_url must be an https:// address without a user name, query or fragment"

object=$(printf '%s' "$entry" | jq -r '.object')
task_id=$(printf '%s' "$entry" | jq -r '.task_id')
episode=$(printf '%s' "$entry" | jq -r '.wait_episode // ""')

# Fail closed: check the object before the token goes anywhere.
mr=$(printf '%s' "$entry" | jq -c --arg host "$host" '
    [.object | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<project>[A-Za-z0-9_][A-Za-z0-9_.-]*(/[A-Za-z0-9_][A-Za-z0-9_.-]*)+)/-/merge_requests/(?<iid>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$mr" ] || done_with "$object is not a merge request URL on $host; not fetched"
project=$(printf '%s' "$mr" | jq -r '.project')
iid=$(printf '%s' "$mr" | jq -r '.iid')
encoded=$(printf '%s' "$project" | jq -R -r '@uri')
name=$(printf '%s-%s' "$project" "$iid" | tr '/' '-')
base="projects/$encoded/merge_requests/$iid"

if [ -z "${FAKE_SOURCE_DIR:-}" ]; then
    [ -n "${GITLAB_TOKEN:-}" ] || fail "GITLAB_TOKEN is not set: give the observer the secret"
fi

quote_config() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

# fetch NAME PATH: the JSON answer of GET API_URL/PATH. The token goes to
# curl on stdin (--config -), never on its command line. Returns 2 when the
# object is gone or the token may not read it (404, 403 or 410, not a rate
# limit), 1 for any other failure.
fetch() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        if [ -f "$FAKE_SOURCE_DIR/$1.status" ]; then
            code=$(cat "$FAKE_SOURCE_DIR/$1.status")
            body=$(cat "$FAKE_SOURCE_DIR/$1.json" 2>/dev/null || true)
        else
            cat "$FAKE_SOURCE_DIR/$1.json"
            return
        fi
    else
        response=$(
            printf 'header = "PRIVATE-TOKEN: %s"\n' "$(quote_config "$GITLAB_TOKEN")" |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --header 'Accept: application/json' \
                    --write-out '\n%{http_code}' \
                    "$api_url/$2"
        ) || return 1
        code=$(printf '%s\n' "$response" | tail -n 1)
        body=$(printf '%s\n' "$response" | sed '$d')
    fi
    case $code in
    2??)
        printf '%s\n' "$body"
        return 0
        ;;
    403)
        if printf '%s' "$body" | grep -qi 'rate limit'; then
            printf '%s: HTTP 403 (rate limit) for %s\n' "$me" "$2" >&2
            return 1
        fi
        return 2
        ;;
    404 | 410) return 2 ;;
    *)
        printf '%s: HTTP %s for %s\n' "$me" "$code" "$2" >&2
        return 1
        ;;
    esac
}

# fetched STATUS WHAT: after a failed fetch of WHAT. A merge request that is
# gone or that the token may not read is done; any other failure is an
# error.
fetched() {
    [ "$1" -eq 2 ] || fail "cannot read $2"
    done_with "$2 is gone or the token may not read it"
}

# send TYPE: sends the event TYPE to the task, when the entry asks for that
# type and the task waits. Returns 0 only when the event went to the
# waiting step ("delivered"); 1 when it sent nothing or the server only
# recorded it.
send() {
    [ -n "$episode" ] || return 1
    printf '%s' "$entry" | jq -e --arg type "$1" '.event_types | index([$type]) != null' >/dev/null ||
        return 1
    data=$(jq -n -c --arg url "$object" --argjson iid "$iid" '{mr_url: $url, iid: $iid}')
    answer=$(narsor-cli task event "$task_id" --type "$1" --data "$data" </dev/null) ||
        fail "the event $1 of $object was refused"
    printf '%s\n' "$answer" >&2
    case $answer in
    delivered*) return 0 ;;
    *) return 1 ;;
    esac
}

mr=$(fetch "mr-$name" "$base") || fetched $? "$object"
approvals=$(fetch "approvals-$name" "$base/approvals") || fetched $? "the approvals of $object"
reviewers=$(fetch "reviewers-$name" "$base/reviewers") || fetched $? "the reviewers of $object"

if printf '%s' "$reviewers" | jq -e 'any(.[]; .state == "requested_changes")' >/dev/null; then
    send review.changes_requested && exit "$DONE"
elif printf '%s' "$approvals" | jq -e '.approved == true' >/dev/null; then
    send review.approved && exit "$DONE"
fi

state=$(printf '%s' "$mr" | jq -r '.state')
case $state in
merged | closed)
    send "$state" || true
    exit "$DONE"
    ;;
esac
exit "$AGAIN"
