#!/bin/sh
# Narsor observer example: answers a review gate when the reviewer of a
# GitLab merge request is done. Read docs/user/observers.md first.
#
# Create the observer once, as the owner. It serves the queue of the gates
# that wait for a review (a `watch` block on a `decision` step with
# `by: human`, docs/dev/workflow-schema.md):
#
#   narsor-cli observer create gitlab-mr \
#     --queue mr-review --mode on_episodes --cron '*/5 * * * *' \
#     --secret gitlab-token=GITLAB_TOKEN \
#     --condition '{"host": "gitlab.example.com"}'
#   narsor-cli observer script set gitlab-mr --file gitlab-mr-review.sh
#   narsor-cli observer approve gitlab-mr --version 1
#
# Condition (a JSON object, given to the script as it is):
#   host     the host of the merge request URLs. Required.
#   api_url  the https:// address of the REST API. Default:
#            https://HOST/api/v4.
#
# Secrets: GITLAB_TOKEN, a token with the read_api scope.
#
# Params of the gate (`watch.params`):
#   mr_url   the merge request URL:
#              https://HOST/GROUP/PROJECT/-/merge_requests/IID
#            (subgroups allowed). Required.
#   label    the label that marks the merge request as "in review".
#            Required. A workflow step puts it on the merge request before
#            the gate starts; the reviewer removes it when the review
#            feedback is on the merge request.
#   choice   the choice of the gate the script answers. Default:
#            check_feedback.
#
#   watch:
#     queue: mr-review
#     params:
#       mr_url: "{{ context.workspace.code.pr_url }}"
#       label: narsor-review
#       choice: check_feedback
#
# One run checks one queue entry (the input's `entry`). When the merge
# request is open, does not carry the label, and changed after the gate
# started (the label was removed while the task waits), the script answers
# the gate:
#
#   narsor-cli task decide TASK --episode EPISODE --choice CHOICE \
#     --reason "the label LABEL was removed from MR_URL"
#
# The answer has no variables and no artifacts. A label that is gone, but
# with no change of the merge request since the gate started, means the
# workflow did not put it there: the script does not answer, and the person
# chooses at the gate.
#
# Several observers may serve one queue (for example this one and
# github-pr-review.sh). Before it uses the token, the script checks mr_url:
# a URL that is not a merge request URL on `host` is not its entry. It
# exits 0 with no answer and never fetches the URL, so the token is not
# sent for it (fail closed).
#
# Exit code (the script prints nothing on stdout):
#   0  the run is done: the script answered the gate, or it has nothing to
#      answer in this run (the label is still there, the merge request is
#      not open, or mr_url is not a merge request URL on `host`).
#   2  an error (a bad input, condition or param, no token, a source that
#      does not answer or refuses the token, a rate limit, a refused
#      answer). Narsor records it for the entry and runs the script for the
#      entry again in the next run.
# No exit code removes the entry: it goes when the gate exits.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answer from
# "$FAKE_SOURCE_DIR/mr-NAME.json", where NAME is GROUP-PROJECT-IID (every /
# of the project path a -), and does not use the network. A file
# mr-NAME.status next to it holds the HTTP status to act as if the source
# gave (for example 404).

set -eu

me=gitlab-mr-review

# The exit codes of the contract.
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
printf '%s' "$input" | jq -e '.contract_version == 2' >/dev/null ||
    fail "the input is not version 2 of the script contract"
entry=$(printf '%s' "$input" | jq -c '.entry // empty')
[ -n "$entry" ] || fail "the input has no queue entry: create the observer with --mode on_episodes"

host=$(printf '%s' "$input" | jq -r '.condition.host // ""')
printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.host must be a host name"
api_url=$(printf '%s' "$input" | jq -r --arg host "$host" '
    .condition.api_url // ("https://" + $host + "/api/v4") | sub("/+$"; "")')
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.api_url must be an https:// address without a user name, query or fragment"

param() {
    printf '%s' "$entry" | jq -r --arg name "$1" '.params[$name] // ""'
}

mr_url=$(param mr_url)
task_id=$(printf '%s' "$entry" | jq -r '.task_id')
episode=$(printf '%s' "$entry" | jq -r '.episode')
started=$(printf '%s' "$entry" | jq -r '.episode_started_at')

# Not our entry: check mr_url before the token goes anywhere.
mr=$(jq -n -c --arg url "$mr_url" --arg host "$host" '
    [$url | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<project>[A-Za-z0-9_][A-Za-z0-9_.-]*(/[A-Za-z0-9_][A-Za-z0-9_.-]*)+)/-/merge_requests/(?<iid>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$mr" ] || done_with "'$mr_url' is not a merge request URL on $host; not mine, not fetched"

label=$(param label)
[ -n "$label" ] || fail "the gate has no param label: the script cannot tell when the review is done"
choice=$(param choice)
[ -n "$choice" ] || choice=check_feedback
printf '%s' "$entry" | jq -e --arg choice "$choice" 'any(.choices[]; .name == $choice)' >/dev/null ||
    fail "the gate $(printf '%s' "$entry" | jq -r '.step') has no choice $choice"

project=$(printf '%s' "$mr" | jq -r '.project')
iid=$(printf '%s' "$mr" | jq -r '.iid')
encoded=$(printf '%s' "$project" | jq -R -r '@uri')
name=$(printf '%s-%s' "$project" "$iid" | tr '/' '-')

if [ -z "${FAKE_SOURCE_DIR:-}" ]; then
    [ -n "${GITLAB_TOKEN:-}" ] || fail "GITLAB_TOKEN is not set: give the observer the secret"
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
            printf 'header = "PRIVATE-TOKEN: %s"\n' "$(quote_config "$GITLAB_TOKEN")" |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --header 'Accept: application/json' \
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

answer=$(fetch "mr-$name" "projects/$encoded/merge_requests/$iid") || exit "$ERROR"
printf '%s' "$answer" | jq -e 'type == "object" and (.labels | type) == "array"' >/dev/null ||
    fail "the answer for $mr_url is not a merge request"

state=$(printf '%s' "$answer" | jq -r '.state')
[ "$state" = opened ] || done_with "$mr_url is $state; nothing to answer"
if printf '%s' "$answer" | jq -e --arg label "$label" 'any(.labels[]; . == $label)' >/dev/null; then
    done_with "$mr_url still has the label $label"
fi
# Did the merge request change at or after the start of the gate? Both
# times are UTC (GitLab and Narsor write them so).
changed=$(printf '%s' "$answer" | jq -r --arg started "$started" '
    def seconds: sub("\\.[0-9]+"; "") | sub("[+-]00:00$"; "Z") | fromdateiso8601;
    if (.updated_at | seconds) >= ($started | seconds) then "yes" else "no" end') ||
    fail "cannot compare updated_at of $mr_url with the start of the gate"
[ "$changed" = yes ] ||
    done_with "$mr_url has no label $label, but it did not change since the gate started"

narsor-cli task decide "$task_id" --episode "$episode" --choice "$choice" \
    --reason "the label $label was removed from $mr_url" </dev/null >&2 ||
    fail "the answer $choice for $mr_url was refused"
exit "$DONE"
