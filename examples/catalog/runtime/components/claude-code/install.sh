# Installs Claude Code system-wide, to /usr/local/bin/claude, and turns its
# self-update off: the image, not the tool, decides the version.
# Needs curl, jq and ca-certificates (the base-tools component).
# NARSOR_COMPONENT_VERSION is the output of version.sh.
set -eu
base=https://downloads.claude.ai/claude-code-releases
version="${NARSOR_COMPONENT_VERSION:?the version script gives the version}"
case "$(uname -m)" in
    x86_64|amd64) platform=linux-x64 ;;
    aarch64|arm64) platform=linux-arm64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
checksum=$(curl -fsSL "$base/$version/manifest.json" \
    | jq -r --arg platform "$platform" '.platforms[$platform].checksum // empty')
[ -n "$checksum" ] || { echo "no $platform build of $version" >&2; exit 1; }
curl -fsSL -o /tmp/claude "$base/$version/$platform/claude"
echo "$checksum  /tmp/claude" | sha256sum -c -
install -m 0755 /tmp/claude /usr/local/bin/claude
rm -f /tmp/claude
# Managed settings apply to every user; their `env` reaches every session.
mkdir -p /etc/claude-code
cat > /etc/claude-code/managed-settings.json <<'JSON'
{
  "env": {
    "DISABLE_AUTOUPDATER": "1"
  }
}
JSON
