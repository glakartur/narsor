#!/bin/sh
# Narsor observer example: events for tasks that wait for the review of a
# GitHub pull request. Read docs/user/observers.md first.
#
# Create the observer once, as the owner. It looks only at the pull
# requests on its watch list; a `wait` step with a `watch` block puts one
# there while the task waits (docs/dev/workflow-schema.md, WaitAction):
#
#   narsor-cli observer create pr-review --source github.com \
#     --deliver-events --allow-prefix https://github.com/ \
#     --secret github-token=GITHUB_TOKEN \
#     --condition '{"host": "github.com", "api_url": "https://api.github.com"}'
#   narsor-cli observer script set pr-review --file github-pr-review.sh
#   narsor-cli observer approve pr-review --version 1
#
# Condition (a JSON object, given to the script as it is):
#   host     the host of the pull request URLs. Default: github.com.
#   api_url  the https:// address of the REST API. Default:
#            https://api.github.com (GitHub Enterprise:
#            https://HOST/api/v3).
#
# Secrets: GITHUB_TOKEN, a token that can read the pull requests.
#
# One run checks one watch entry (the input's `watch`). Its object is a
# pull request URL:
#   https://github.com/OWNER/REPO/pull/NUMBER
# Before it uses the token, the script checks the object: an object that is
# not a pull request URL on `host` is never fetched, so the token is not
# sent for it (fail closed). The control plane checks the objects too
# (--allow-prefix); this check is the second line of defence.
#
# Events (the entry gets only the types in its event_types):
#   review.approved            the latest review decision is an approval
#   review.changes_requested   the latest review decision asks for changes
#   merged                     the pull request is merged
#   closed                     the pull request is closed without a merge
# Event data: {"pr_url": URL, "number": N}. Filter a `wait` step on pr_url.
#
# Exit code (the script prints nothing on stdout):
#   0  done: Narsor removes the entry. A review event went to the waiting
#      step of the task; or the pull request is merged or closed; or the
#      object is not a pull request URL on `host`, or the pull request is
#      gone or the token may not read it (HTTP 404, 403 or 410).
#   1  check the entry again in the next run: no review decision, the task
#      does not wait (no wait_episode), or the task does not wait for this
#      event (narsor-cli says "recorded", not "delivered").
#   2  an error (a bad input or condition, no token, a source that does not
#      answer, a rate limit, a refused event). Narsor records it and checks
#      the entry again in the next run.
#
# When a task waits again for the same pull request (a review loop), its
# `wait` step puts the pull request on the watch list again, and the old
# review decision on GitHub still matches. Mark the pull request before it
# goes back on the list (for example with a label that the script looks
# for); this example does not do that.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answers from
# "$FAKE_SOURCE_DIR/pr-OWNER-REPO-NUMBER.json" and
# "$FAKE_SOURCE_DIR/reviews-OWNER-REPO-NUMBER.json" and does not use the
# network. A file NAME.status next to an answer holds the HTTP status to
# act as if the source gave (for example 404).

set -eu

me=github-pr-review

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

host=$(printf '%s' "$input" | jq -r '.condition.host // "github.com"')
api_url=$(printf '%s' "$input" | jq -r '.condition.api_url // "https://api.github.com" | sub("/+$"; "")')

printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.host must be a host name"
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.api_url must be an https:// address without a user name, query or fragment"

object=$(printf '%s' "$entry" | jq -r '.object')
task_id=$(printf '%s' "$entry" | jq -r '.task_id')
episode=$(printf '%s' "$entry" | jq -r '.wait_episode // ""')

# Fail closed: check the object before the token goes anywhere.
pr=$(printf '%s' "$entry" | jq -c --arg host "$host" '
    [.object | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<owner>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/(?<repo>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/pull/(?<number>[0-9]+)\\z"
    )] | map(select(.host == $host)) | .[0] // empty')
[ -n "$pr" ] || done_with "$object is not a pull request URL on $host; not fetched"
owner=$(printf '%s' "$pr" | jq -r '.owner')
repo=$(printf '%s' "$pr" | jq -r '.repo')
number=$(printf '%s' "$pr" | jq -r '.number')
name="$owner-$repo-$number"

if [ -z "${FAKE_SOURCE_DIR:-}" ]; then
    [ -n "${GITHUB_TOKEN:-}" ] || fail "GITHUB_TOKEN is not set: give the observer the secret"
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
            printf 'header = "Authorization: Bearer %s"\n' "$(quote_config "$GITHUB_TOKEN")" |
                curl --silent --show-error --proto =https --max-time 60 \
                    --config - \
                    --header 'Accept: application/vnd.github+json' \
                    --header 'X-GitHub-Api-Version: 2022-11-28' \
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

# fetched STATUS WHAT: after a failed fetch of WHAT. A pull request that is
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
    data=$(jq -n -c --arg url "$object" --argjson number "$number" '{pr_url: $url, number: $number}')
    answer=$(narsor-cli task event "$task_id" --type "$1" --data "$data" </dev/null) ||
        fail "the event $1 of $object was refused"
    printf '%s\n' "$answer" >&2
    case $answer in
    delivered*) return 0 ;;
    *) return 1 ;;
    esac
}

pr=$(fetch "pr-$name" "repos/$owner/$repo/pulls/$number") || fetched $? "$object"
reviews=$(fetch "reviews-$name" "repos/$owner/$repo/pulls/$number/reviews?per_page=100") ||
    fetched $? "the reviews of $object"

decision=$(printf '%s' "$reviews" | jq -r '
    [.[] | select(.state == "APPROVED" or .state == "CHANGES_REQUESTED")]
    | last | .state // ""')
case $decision in
APPROVED) send review.approved && exit "$DONE" ;;
CHANGES_REQUESTED) send review.changes_requested && exit "$DONE" ;;
esac

state=$(printf '%s' "$pr" | jq -r 'if .merged == true or .merged_at != null then "merged" else .state end')
case $state in
merged | closed)
    send "$state" || true
    exit "$DONE"
    ;;
esac
exit "$AGAIN"
