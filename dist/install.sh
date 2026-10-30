#!/usr/bin/env bash
set -euo pipefail

repository="glakartur/narsor"
target="x86_64-unknown-linux-gnu"
archive="narsor-linux-x86_64-gnu.tar.gz"
version="${NARSOR_VERSION:-latest}"
if [[ "${1:-}" == "latest" || "${1:-}" == v* ]]; then
    version="$1"
    shift
fi

fail() {
    printf 'narsor installer: %s\n' "$*" >&2
    exit 1
}

[[ "$(uname -s)" == "Linux" ]] || fail "this installer supports Linux only"
[[ "$(uname -m)" == "x86_64" ]] || fail "this release supports x86_64 only"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"

if [[ "$version" != "latest" ]]; then
    [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must be latest or vMAJOR.MINOR.PATCH"
    base_url="https://github.com/$repository/releases/download/$version"
else
    base_url="https://github.com/$repository/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
    download() {
            curl --fail --location --proto '=https' --proto-redir '=https' --silent --show-error \
                "$1" --output "$2"
    }
elif command -v wget >/dev/null 2>&1; then
    download() {
        wget --https-only --quiet "$1" --output-document="$2"
    }
else
    fail "curl or wget is required"
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
download "$base_url/$archive" "$work_dir/$archive"
download "$base_url/$archive.sha256" "$work_dir/$archive.sha256"

(cd "$work_dir" && sha256sum --check "$archive.sha256") || fail "release archive checksum verification failed"

while IFS= read -r entry; do
    case "$entry" in
        manifest.json|narsor|narsor-cli|narsor-server|narsor-controller|narsor-runner) ;;
        *) fail "unexpected path in release archive: $entry" ;;
    esac
done < <(tar -tzf "$work_dir/$archive")

tar -xzf "$work_dir/$archive" -C "$work_dir"
[[ -f "$work_dir/manifest.json" && ! -L "$work_dir/manifest.json" ]] || fail "release archive has no regular manifest"
grep -Fq "\"target\": \"$target\"" "$work_dir/manifest.json" || fail "release archive target does not match $target"
for binary in narsor narsor-cli narsor-server narsor-controller narsor-runner; do
    [[ -f "$work_dir/$binary" && ! -L "$work_dir/$binary" && -x "$work_dir/$binary" ]] \
        || fail "release archive is missing a regular executable: $binary"
done
"$work_dir/narsor-cli" install --source-dir "$work_dir" "$@"
