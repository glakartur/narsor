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
#   narsor-cli observer dry-run mr-review
#   narsor-cli observer approve mr-review --version 1
#
# Condition (a JSON object, given to the script as it is):
#   host     the host of the merge request URLs. Required.
#   api_url  the https:// address of the REST API. Default:
#            https://HOST/api/v4.
#
# Secrets: GITLAB_TOKEN, a token with the read_api scope.
#
# A watched object is a merge request URL:
#   https://HOST/GROUP/PROJECT/-/merge_requests/IID
# (subgroups allowed). Before it uses the token, the script checks every
# object: an object that is not a merge request URL on `host` is never
# fetched, so the token is not sent for it (fail closed). The script logs it
# on stderr and reports it in closed_watches, so its entry is removed; the
# other entries are handled as usual. The control plane checks the objects
# too (--allow-prefix); this check is the second line of defence.
#
# Events (a watch entry gets only the types in its event_types):
#   review.approved            the merge request has its required approvals
#   review.changes_requested   a reviewer asked for changes
#   merged                     the merge request is merged
#   closed                     the merge request is closed without a merge
# Event data: {"mr_url": URL, "iid": N}. Filter a `wait` step on mr_url.
#
# A task gets events only while it waits (the entry has a wait_episode).
# The event key is GROUP/PROJECT!IID:TYPE:EPISODE (for example
# acme/api!17:review.approved:<episode>), so a task gets each type at most
# once per wait, and again in its next wait (a review loop). A merged or
# closed merge request is reported in closed_watches: its entry is
# removed. The state is kept as it was given.
#
# A merge request the source does not find or the token may not read (HTTP
# 404, 403 or 410) is skipped the same way: logged and reported in
# closed_watches. A 403 for a rate limit is not: it fails the run. Only a
# problem of the whole run (a bad input or condition, no token, a source
# that does not answer) makes the script exit non-zero.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answers from
# "$FAKE_SOURCE_DIR/mr-NAME.json", "$FAKE_SOURCE_DIR/approvals-NAME.json"
# and "$FAKE_SOURCE_DIR/reviewers-NAME.json", where NAME is
# GROUP-PROJECT-IID (every / of the project path a -), and does not use the
# network. A file NAME.status next to an answer holds the HTTP status to
# act as if the source gave (for example 404).

set -eu

me=gitlab-mr-review

fail() {
    printf '%s: %s\n' "$me" "$*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || fail "jq is not installed"

input=$(cat)
printf '%s' "$input" | jq -e '.contract_version == 1' >/dev/null ||
    fail "the input is not version 1 of the script contract"

host=$(printf '%s' "$input" | jq -r '.condition.host // ""')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.host must be a host name"
api_url=$(printf '%s' "$input" | jq -r --arg host "$host" '
    .condition.api_url // ("https://" + $host + "/api/v4") | sub("/+$"; "")')
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.api_url must be an https:// address without a user name, query or fragment"

# One line per watch entry: the entry and the parts of its URL. An object
# that is not a merge request URL on `host` has no parts.
watches=$(printf '%s' "$input" | jq -c --arg host "$host" '
    .watches[]
    | . + {mr: ([.object | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<project>[A-Za-z0-9_][A-Za-z0-9_.-]*(/[A-Za-z0-9_][A-Za-z0-9_.-]*)+)/-/merge_requests/(?<iid>[0-9]+)\\z"
      )] | map(select(.host == $host)) | .[0])}')

# Fail closed: check every object before the token goes anywhere. An
# object that does not fit is never fetched; its entry is closed.
refused=$(printf '%s' "$watches" | jq -r 'select(.mr == null) | .object')
if [ -n "$refused" ]; then
    printf '%s: not a merge request URL on %s, skipped and closed:\n%s\n' "$me" "$host" "$refused" >&2
fi
closed=$(printf '%s' "$watches" | jq -s -c '[.[] | select(.mr == null) | .object] | unique')
watches=$(printf '%s' "$watches" | jq -c 'select(.mr != null)')

if [ -z "${FAKE_SOURCE_DIR:-}" ] && [ -n "$watches" ]; then
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

# close_entry: reports the current entry in closed_watches.
close_entry() {
    closed=$(printf '%s' "$closed" |
        jq -c --arg object "$object" 'if index([$object]) == null then . + [$object] else . end')
}

# skip_entry STATUS WHAT: after a failed fetch of WHAT of the current entry.
# A merge request that is gone or that the token may not read is skipped and
# its entry closed; any other failure fails the run.
skip_entry() {
    [ "$1" -eq 2 ] || fail "cannot read $2"
    printf '%s: %s is gone or the token may not read it; skipped and closed\n' "$me" "$2" >&2
    close_entry
}

# send TYPE: delivers the event TYPE to the task of the current entry, when
# the entry asks for that type and the task waits.
send() {
    [ -n "$episode" ] || return 0
    printf '%s' "$entry" | jq -e --arg type "$1" '.event_types | index([$type]) != null' >/dev/null ||
        return 0
    key="$project!$iid:$1:$episode"
    data=$(jq -n -c --arg url "$object" --argjson iid "$iid" '{mr_url: $url, iid: $iid}')
    # stdout is the result document: the messages of narsor-cli go to stderr.
    narsor-cli task event "$task_id" --type "$1" --key "$key" --data "$data" </dev/null >&2 ||
        fail "the event $1 of $object was not delivered"
}

while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    object=$(printf '%s' "$entry" | jq -r '.object')
    task_id=$(printf '%s' "$entry" | jq -r '.task_id')
    episode=$(printf '%s' "$entry" | jq -r '.wait_episode // ""')
    project=$(printf '%s' "$entry" | jq -r '.mr.project')
    iid=$(printf '%s' "$entry" | jq -r '.mr.iid')
    encoded=$(printf '%s' "$project" | jq -R -r '@uri')
    name=$(printf '%s-%s' "$project" "$iid" | tr '/' '-')
    base="projects/$encoded/merge_requests/$iid"

    mr=$(fetch "mr-$name" "$base") || {
        skip_entry $? "$object"
        continue
    }
    approvals=$(fetch "approvals-$name" "$base/approvals") || {
        skip_entry $? "the approvals of $object"
        continue
    }
    reviewers=$(fetch "reviewers-$name" "$base/reviewers") || {
        skip_entry $? "the reviewers of $object"
        continue
    }

    if printf '%s' "$reviewers" | jq -e 'any(.[]; .state == "requested_changes")' >/dev/null; then
        send review.changes_requested
    elif printf '%s' "$approvals" | jq -e '.approved == true' >/dev/null; then
        send review.approved
    fi

    state=$(printf '%s' "$mr" | jq -r '.state')
    case $state in
    merged | closed)
        send "$state"
        close_entry
        ;;
    esac
done <<EOF
$watches
EOF

printf '%s' "$input" | jq -c --argjson closed "$closed" '{state: (.state // {}), closed_watches: $closed}'
