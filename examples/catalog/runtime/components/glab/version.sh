# Prints the latest glab release, without the leading `v`.
# Runs on the host (not in the image) with only PATH, HOME and LANG.
set -eu
curl -fsSL https://gitlab.com/api/v4/projects/gitlab-org%2Fcli/releases/permalink/latest \
    | sed -n 's/.*"tag_name":"v\{0,1\}\([^"]*\)".*/\1/p'
