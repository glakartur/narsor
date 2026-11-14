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
#   narsor-cli observer dry-run pr-review
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
# A watched object is a pull request URL:
#   https://github.com/OWNER/REPO/pull/NUMBER
# Before it uses the token, the script checks every object: an object that
# is not a pull request URL on `host` fails the whole run, and the token is
# not sent anywhere (fail closed). The control plane checks the objects too
# (--allow-prefix); this check is the second line of defence.
#
# Events (a watch entry gets only the types in its event_types):
#   review.approved            the latest review decision is an approval
#   review.changes_requested   the latest review decision asks for changes
#   merged                     the pull request is merged
#   closed                     the pull request is closed without a merge
# Event data: {"pr_url": URL, "number": N}. Filter a `wait` step on pr_url.
#
# A task gets events only while it waits (the entry has a wait_episode).
# The event key is OWNER/REPO#NUMBER:TYPE:EPISODE, so a task gets each type
# at most once per wait, and again in its next wait (a review loop).
# A merged or closed pull request is reported in closed_watches: its entry
# is removed. The state is kept as it was given.
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answers from
# "$FAKE_SOURCE_DIR/pr-OWNER-REPO-NUMBER.json" and
# "$FAKE_SOURCE_DIR/reviews-OWNER-REPO-NUMBER.json" and does not use the
# network.

set -eu

me=github-pr-review

fail() {
    printf '%s: %s\n' "$me" "$*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || fail "jq is not installed"

input=$(cat)
printf '%s' "$input" | jq -e '.contract_version == 1' >/dev/null ||
    fail "the input is not version 1 of the script contract"

host=$(printf '%s' "$input" | jq -r '.condition.host // "github.com"')
api_url=$(printf '%s' "$input" | jq -r '.condition.api_url // "https://api.github.com" | sub("/+$"; "")')

printf '%s' "$host" | jq -R -e 'test("\\A[A-Za-z0-9.-]+(:[0-9]+)?\\z")' >/dev/null ||
    fail "condition.host must be a host name"
printf '%s' "$api_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.api_url must be an https:// address without a user name, query or fragment"

# One line per watch entry: the entry and the parts of its URL. An object
# that is not a pull request URL on `host` has no parts.
watches=$(printf '%s' "$input" | jq -c --arg host "$host" '
    .watches[]
    | . + {pr: ([.object | capture(
        "\\Ahttps://(?<host>[A-Za-z0-9.:-]+)/(?<owner>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/(?<repo>[A-Za-z0-9_-][A-Za-z0-9_.-]*)/pull/(?<number>[0-9]+)\\z"
      )] | map(select(.host == $host)) | .[0])}')

# Fail closed: check every object before the token goes anywhere.
refused=$(printf '%s' "$watches" | jq -r 'select(.pr == null) | .object')
if [ -n "$refused" ]; then
    printf '%s: not a pull request URL on %s, refused:\n%s\n' "$me" "$host" "$refused" >&2
    exit 1
fi

if [ -z "${FAKE_SOURCE_DIR:-}" ] && [ -n "$watches" ]; then
    [ -n "${GITHUB_TOKEN:-}" ] || fail "GITHUB_TOKEN is not set: give the observer the secret"
fi

quote_config() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

# fetch NAME PATH: the JSON answer of GET API_URL/PATH. The token goes to
# curl on stdin (--config -), never on its command line.
fetch() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        cat "$FAKE_SOURCE_DIR/$1.json"
        return
    fi
    printf 'header = "Authorization: Bearer %s"\n' "$(quote_config "$GITHUB_TOKEN")" |
        curl --silent --show-error --fail --proto =https --max-time 60 \
            --config - \
            --header 'Accept: application/vnd.github+json' \
            --header 'X-GitHub-Api-Version: 2022-11-28' \
            "$api_url/$2"
}

# send TYPE: delivers the event TYPE to the task of the current entry, when
# the entry asks for that type and the task waits.
send() {
    [ -n "$episode" ] || return 0
    printf '%s' "$entry" | jq -e --arg type "$1" '.event_types | index([$type]) != null' >/dev/null ||
        return 0
    key="$owner/$repo#$number:$1:$episode"
    data=$(jq -n -c --arg url "$object" --argjson number "$number" '{pr_url: $url, number: $number}')
    # stdout is the result document: the messages of narsor-cli go to stderr.
    narsor-cli task event "$task_id" --type "$1" --key "$key" --data "$data" </dev/null >&2 ||
        fail "the event $1 of $object was not delivered"
}

closed='[]'
while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    object=$(printf '%s' "$entry" | jq -r '.object')
    task_id=$(printf '%s' "$entry" | jq -r '.task_id')
    episode=$(printf '%s' "$entry" | jq -r '.wait_episode // ""')
    owner=$(printf '%s' "$entry" | jq -r '.pr.owner')
    repo=$(printf '%s' "$entry" | jq -r '.pr.repo')
    number=$(printf '%s' "$entry" | jq -r '.pr.number')
    name="$owner-$repo-$number"

    pr=$(fetch "pr-$name" "repos/$owner/$repo/pulls/$number") ||
        fail "cannot read $object"
    reviews=$(fetch "reviews-$name" "repos/$owner/$repo/pulls/$number/reviews?per_page=100") ||
        fail "cannot read the reviews of $object"

    decision=$(printf '%s' "$reviews" | jq -r '
        [.[] | select(.state == "APPROVED" or .state == "CHANGES_REQUESTED")]
        | last | .state // ""')
    case $decision in
    APPROVED) send review.approved ;;
    CHANGES_REQUESTED) send review.changes_requested ;;
    esac

    state=$(printf '%s' "$pr" | jq -r 'if .merged == true or .merged_at != null then "merged" else .state end')
    case $state in
    merged | closed)
        send "$state"
        closed=$(printf '%s' "$closed" | jq -c --arg object "$object" 'if index([$object]) == null then . + [$object] else . end')
        ;;
    esac
done <<EOF
$watches
EOF

printf '%s' "$input" | jq -c --argjson closed "$closed" '{state: (.state // {}), closed_watches: $closed}'
