#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="x86_64-unknown-linux-gnu"
target_dir="${CARGO_TARGET_DIR:-"$repo_root/target"}/$target/release"
output_dir="${NARSOR_DIST_DIR:-"$repo_root/dist/release"}"
archive_name="narsor-linux-x86_64-gnu.tar.gz"
binaries=(narsor narsor-cli narsor-server narsor-controller narsor-runner)

fail() {
    printf 'package-linux: %s\n' "$*" >&2
    exit 1
}

cargo build --workspace --bins --release --locked --target "$target"

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
    printf '{\n  "target": "%s",\n  "binaries": [\n' "$target"
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

printf 'Packaged %s and checksum in %s\n' "$archive_name" "$output_dir"
