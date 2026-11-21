# Prints the Claude Code version to install: the `stable` channel.
# Runs on the host (not in the image) with only PATH, HOME and LANG.
set -eu
curl -fsSL https://downloads.claude.ai/claude-code-releases/stable
