#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="x86_64-unknown-linux-gnu"
target_dir="${CARGO_TARGET_DIR:-"$repo_root/target"}/$target/release"
output_dir="${NARSOR_DIST_DIR:-"$repo_root/dist/release"}"
archive_name="narsor-linux-x86_64-gnu.tar.gz"
binaries=(narsor narsor-cli narsor-web narsor-server narsor-controller narsor-runner)

fail() {
    printf 'package-linux: %s\n' "$*" >&2
    exit 1
}

# The version every binary reports (`--version`): NARSOR_BUILD_VERSION when the
# release workflow sets it from the tag, else the version of the crate.
if [[ -n "${NARSOR_BUILD_VERSION:-}" ]]; then
    [[ "$NARSOR_BUILD_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$ ]] \
        || fail "NARSOR_BUILD_VERSION must be MAJOR.MINOR.PATCH without a leading v: $NARSOR_BUILD_VERSION"
    export NARSOR_BUILD_VERSION
fi

cargo build --workspace --bins --release --locked --target "$target"
version="$("$target_dir/narsor-cli" --version)"
version="${version#narsor-cli }"
[[ -z "${NARSOR_BUILD_VERSION:-}" || "$version" == "$NARSOR_BUILD_VERSION" ]] \
    || fail "narsor-cli reports $version, expected $NARSOR_BUILD_VERSION"

for name in "${binaries[@]}"; do
    binary="$target_dir/$name"
    [[ -x "$binary" ]] || fail "required executable is missing: $binary"

    header="$(readelf -h "$binary")" || fail "cannot inspect ELF executable: $name"
    [[ "$header" == *"Advanced Micro Devices X86-64"* ]] || fail "not an x86_64 ELF executable: $name"

    program_headers="$(readelf -l "$binary")" || fail "cannot inspect ELF program headers: $name"
    [[ "$program_headers" == *"ld-linux-x86-64.so.2"* ]] || fail "executable does not use the GNU x86_64 dynamic loader: $name"
done

staging_dir="$(mktemp -d)"
trap 'rm -rf "$staging_dir"' EXIT

for name in "${binaries[@]}"; do
    install -m 0755 "$target_dir/$name" "$staging_dir/$name"
done

{
    printf '{\n  "target": "%s",\n  "version": "%s",\n  "binaries": [\n' "$target" "$version"
    for index in "${!binaries[@]}"; do
        separator=","
        [[ "$index" -eq "$((${#binaries[@]} - 1))" ]] && separator=""
        printf '    "%s"%s\n' "${binaries[$index]}" "$separator"
    done
    printf '  ]\n}\n'
} > "$staging_dir/manifest.json"

mkdir -p "$output_dir"
archive_tmp="$output_dir/.$archive_name.tmp.$$"
checksum_tmp="$output_dir/.$archive_name.sha256.tmp.$$"
trap 'rm -rf "$staging_dir"; rm -f "$archive_tmp" "$checksum_tmp"' EXIT

tar -C "$staging_dir" -czf "$archive_tmp" manifest.json "${binaries[@]}"
sha256sum "$archive_tmp" | sed "s#  .*#  $archive_name#" > "$checksum_tmp"
mv "$archive_tmp" "$output_dir/$archive_name"
mv "$checksum_tmp" "$output_dir/$archive_name.sha256"

printf 'Packaged %s (version %s) and checksum in %s\n' "$archive_name" "$version" "$output_dir"
