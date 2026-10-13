#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$REPO_ROOT"

pids=()

# Stop only Narsor binaries built in this workspace. This avoids killing an
# unrelated installation with the same executable name.
for proc_exe in /proc/[0-9]*/exe; do
    [[ -e "$proc_exe" ]] || continue
    exe_path="$(readlink -f -- "$proc_exe" 2>/dev/null || true)"
    case "$exe_path" in
        "$REPO_ROOT"/target/*/narsor|\
        "$REPO_ROOT"/target/*/narsor-cli|\
        "$REPO_ROOT"/target/*/narsor-server|\
        "$REPO_ROOT"/target/*/narsor-runner|\
        "$REPO_ROOT"/target/*/narsor-controller)
            pid="${proc_exe#/proc/}"
            pid="${pid%%/*}"
            [[ "$pid" == "$$" ]] || pids+=("$pid")
            ;;
    esac
done

if ((${#pids[@]})); then
    printf 'Zatrzymuję procesy Narsor z tego repo: %s\n' "${pids[*]}"
    for pid in "${pids[@]}"; do
        kill -TERM "$pid" 2>/dev/null || true
    done

    for _ in {1..50}; do
        remaining=()
        for pid in "${pids[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                remaining+=("$pid")
            fi
        done
        ((${#remaining[@]} == 0)) && break
        sleep 0.1
    done

    for pid in "${remaining[@]:-}"; do
        kill -KILL "$pid" 2>/dev/null || true
    done
else
    echo 'Nie znaleziono działających procesów Narsor z tego repo.'
fi

cargo build --workspace --bins --release
exec cargo run --release -p narsor --bin narsor
