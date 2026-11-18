#!/bin/sh
# Narsor observer example: one task for every new Jira issue that a JQL
# query selects. Read docs/user/observers.md first.
#
# Create the observer once, as the owner (the workflow `triage` has an input
# `ticket`; the script sends the issue key in the data field `key`):
#
#   narsor-cli observer create jira-new \
#     --workflow triage --map ticket=key \
#     --secret jira-token=JIRA_TOKEN --task-limit 10 \
#     --condition '{"base_url": "https://jira.example.com",
#                   "jql": "project = ABC AND issuetype = Bug",
#                   "fields": ["key"]}'
#   narsor-cli observer script set jira-new --file jira-new-issues.sh
#   narsor-cli observer approve jira-new --version 1
#
# Condition (a JSON object, given to the script as it is):
#   base_url     https:// address of the Jira server. Required.
#   jql          the query, without ORDER BY. Required.
#   fields       the data fields sent for each issue. Each one must be a
#                field that the observer maps (--map INPUT=FIELD): the
#                control plane refuses a field it does not map. Available:
#                key, summary, issue_type, priority, status, labels, url,
#                description. Default: ["key"].
#   max_results  the most issues one run looks at, 1-100. Default: 10. Keep
#                it at or below the observer's --task-limit.
#   api_path     the search endpoint. Default: /rest/api/2/search.
#
# Secrets: JIRA_TOKEN, a personal access token sent as a Bearer token. When
# the secret JIRA_USER is also given, the script sends JIRA_USER:JIRA_TOKEN
# as basic authentication instead (Jira Cloud).
#
# Personal data: Jira issues can hold data of customers and other people.
# The script sends to Narsor only the fields in `fields`, never the
# reporter, the assignee, comments or attachments. Do not add a field that
# holds personal data. `summary` and `description` are free text written by
# people: send them only when the workflow needs them, and only when you
# know what they hold.
#
# State: {"last_id": N}, the highest Jira issue id the observer handled.
#  * First run without --import-backlog: the script only records the
#    newest matching issue as the starting point and creates no task.
#  * First run with --import-backlog, and every later run: one task for
#    each matching issue with an id above last_id, oldest first, at most
#    max_results per run; the next run continues from there.
# Narsor does not stop a second task for the same issue: the state does.
# When a task is not created, the script stops creating tasks and logs the
# reason on stderr. When the run created a task before that, it prints the
# state up to the last task it created and exits 0, so the next run
# continues from there and repeats no task. When the run created no task,
# it exits 1: Narsor records the error, the observer counts it, and a
# refusal that does not go away (an unmapped field, a limit, a policy)
# brings the observer to error (5 failed runs in a row) instead of
# repeating the same issue for ever. The run created nothing, so its
# failure gives no second task. Only when the state of a successful run is
# not saved can an issue get a second task (docs/user/observers.md).
#
# Tests: when FAKE_SOURCE_DIR is set, the script reads the answer of Jira
# from "$FAKE_SOURCE_DIR/search.json" and does not use the network.

set -eu

me=jira-new-issues

fail() {
    printf '%s: %s\n' "$me" "$*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || fail "jq is not installed"

input=$(cat)
printf '%s' "$input" | jq -e '.contract_version == 3' >/dev/null ||
    fail "the input is not version 3 of the script contract"

# read_input FILTER: the value of a jq filter over the input, as text.
read_input() {
    printf '%s' "$input" | jq -r "$1"
}

base_url=$(read_input '.condition.base_url // "" | sub("/+$"; "")')
jql=$(read_input '.condition.jql // ""')
api_path=$(read_input '.condition.api_path // "/rest/api/2/search"')
max_results=$(read_input '.condition.max_results // 10')
fields=$(read_input '.condition.fields // ["key"] | tojson')
first_run=$(read_input '.first_run')
import_backlog=$(read_input '.import_backlog')
last_id=$(read_input '.state.last_id // 0')

# The address the token is sent to: https, a plain host, no user name.
printf '%s' "$base_url" |
    jq -R -e 'test("\\Ahttps://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?\\z")' >/dev/null ||
    fail "condition.base_url must be an https:// address without a user name, query or fragment"
printf '%s' "$api_path" | jq -R -e 'test("\\A/[A-Za-z0-9._~/-]*\\z")' >/dev/null ||
    fail "condition.api_path must be a path that starts with /"
[ -n "$jql" ] || fail "the condition has no jql"
case $max_results in
'' | *[!0-9]*) fail "condition.max_results must be a number from 1 to 100" ;;
esac
[ "$max_results" -ge 1 ] && [ "$max_results" -le 100 ] ||
    fail "condition.max_results must be a number from 1 to 100"
case $last_id in
'' | *[!0-9]*) fail "the state has a last_id that is not a number" ;;
esac
printf '%s' "$fields" | jq -e '
    type == "array" and length > 0
    and all(.[]; . as $field
        | ["key", "summary", "issue_type", "priority", "status", "labels", "url", "description"]
        | index([$field]) != null)' >/dev/null ||
    fail "condition.fields must list some of: key, summary, issue_type, priority, status, labels, url, description"

if [ "$first_run" = true ] && [ "$import_backlog" != true ]; then
    # The first run only records where to start (no task for the backlog).
    query="($jql) ORDER BY id DESC"
    limit=1
else
    query="($jql) AND id > $last_id ORDER BY id ASC"
    limit=$max_results
fi

# curl_config: the authentication as a curl configuration, which curl reads
# on stdin, so that the token is never on a command line (where other users
# could read it with ps).
curl_config() {
    if [ -n "${JIRA_USER:-}" ]; then
        printf 'user = "%s:%s"\n' "$(quote_config "$JIRA_USER")" "$(quote_config "$JIRA_TOKEN")"
    else
        printf 'header = "Authorization: Bearer %s"\n' "$(quote_config "$JIRA_TOKEN")"
    fi
}

quote_config() {
    printf '%s' "$1" | sed 's/[\\"]/\\&/g'
}

search() {
    if [ -n "${FAKE_SOURCE_DIR:-}" ]; then
        cat "$FAKE_SOURCE_DIR/search.json"
        return
    fi
    [ -n "${JIRA_TOKEN:-}" ] || fail "JIRA_TOKEN is not set: give the observer the secret"
    curl_config | curl --silent --show-error --fail --proto =https \
        --max-time 60 --config - --get \
        --header 'Accept: application/json' \
        --data-urlencode "jql=$query" \
        --data-urlencode "fields=summary,issuetype,priority,status,labels,description" \
        --data-urlencode "maxResults=$limit" \
        "$base_url$api_path"
}

response=$(search) || fail "the Jira search failed"
printf '%s' "$response" | jq -e '.issues | type == "array"' >/dev/null ||
    fail "the Jira answer has no list of issues"
printf '%s' "$response" | jq -e '
    all(.issues[]; (.id | tostring | test("\\A[0-9]+\\z"))
        and (.key | type == "string" and test("\\A[A-Z][A-Z0-9_]*-[0-9]+\\z")))' >/dev/null ||
    fail "the Jira answer has an issue without a numeric id or a valid key"

if [ "$first_run" = true ] && [ "$import_backlog" != true ]; then
    printf '%s' "$response" | jq -c '
        {state: {last_id: ([.issues[].id | tonumber] | max // 0)}}'
    exit 0
fi

# One line per issue, oldest first: {"id": …, "key": …, "data": {…}}, the
# data holding only the fields of the condition.
issues=$(printf '%s' "$response" | jq -c \
    --argjson after "$last_id" --argjson limit "$limit" \
    --argjson fields "$fields" --arg base "$base_url" '
    [.issues[] | .id |= tonumber | select(.id > $after)]
    | sort_by(.id) | .[:$limit] | .[]
    | {id, key, data: ({
        key: .key,
        summary: .fields.summary,
        issue_type: .fields.issuetype.name,
        priority: .fields.priority.name,
        status: .fields.status.name,
        labels: .fields.labels,
        url: ($base + "/browse/" + .key),
        description: .fields.description
      } | with_entries(select(.key as $name | $fields | index([$name]) != null)))}')

newest=$last_id
refused=
while IFS= read -r issue; do
    [ -n "$issue" ] || continue
    key=$(printf '%s' "$issue" | jq -r '.key')
    data=$(printf '%s' "$issue" | jq -c '.data')
    # stdout is the result document: the messages of narsor-cli go to stderr.
    if ! narsor-cli task create --data "$data" </dev/null >&2; then
        refused=$key
        break
    fi
    newest=$(printf '%s' "$issue" | jq -r '.id')
done <<EOF
$issues
EOF

if [ -n "$refused" ]; then
    # No task in this run: fail, so the error is recorded and counted.
    [ "$newest" != "$last_id" ] || fail "the task of $refused was not created"
    printf '%s: the task of %s was not created; the next run tries again\n' "$me" "$refused" >&2
fi

jq -n -c --argjson last_id "$newest" '{state: {last_id: $last_id}}'
