#!/usr/bin/env bash
# Instalator wydania Narsor dla Linux x86_64.
#
# Instalacja:    install.sh [wersja] [--role local|frontend|server] [--bin-dir KATALOG]
#                           [--with-frontend] [--update-bashrc] [--non-interactive]
#                           [--stop-running] [--migrate] [--progress]
#                           [opcje narsor-cli init, np. --data-dir, --config-file]
# Odinstalowanie: install.sh --uninstall [--purge] [--bin-dir KATALOG]
#                           [--data-dir KATALOG] [--config-file PLIK] [--non-interactive]
#
# Skrypt pobiera i weryfikuje archiwum, kopiuje binaria i (za zgodą) dopisuje
# katalog do ~/.bashrc. Gdy działają już procesy Narsora, pyta, czy je zatrzymać
# (--stop-running robi to bez pytania). --progress pokazuje pasek postępu
# pobierania archiwum.
# `narsor-cli init` writes the configuration: it asks for the data root
# directory (~/.narsor by default), writes it as global.root and writes
# server.catalog, server.state and server.work relative to it. It does not touch an
# existing configuration: it writes config.yaml.new next to it.
# server.state and server.work are roots: every program keeps its own directory
# below them (state/run/, state/narsor-server/, state/narsor-controller-local/,
# state/narsor-web/, state/narsor/, work/narsor-controller-local/). --purge runs
# `narsor-cli purge`, which removes the whole data directory with all of them and
# lists, but keeps, directories the configuration names outside it.
# For the local role it also asks for the work mode of local tasks (process or
# docker); `--local-mode process|docker` answers that question and is passed
# to `narsor-cli init` like the other init options.
# For the local and server roles it then runs `narsor-migrate`, which moves the
# data of an older version to the per-binary layout once, after a backup in
# <global.root>/backups/. The Narsor processes must be stopped for that; when
# they keep running, the migration is skipped and the command is printed.
# Interactive: narsor-migrate shows its plan and asks. --non-interactive: it
# migrates (with --yes) only with --migrate; otherwise it prints the command.
# Then it runs `narsor-migrate runtime-catalog`, which moves an old runtime
# catalogue ({catalog}/runtime.yaml and {catalog}/components/) into
# {catalog}/runtime/ by renaming, without a backup or a question. While Narsor
# still runs, it offers once more (interactive) to stop it; otherwise the move
# is skipped with the command to run, and the old place keeps working. When
# both layouts exist or a rename fails, it prints the step, then fails with
# --non-interactive and continues otherwise.
set -euo pipefail

repository="glakartur/narsor"
target="x86_64-unknown-linux-gnu"
archive="narsor-linux-x86_64-gnu.tar.gz"
version="${NARSOR_VERSION:-latest}"
all_binaries=(narsor narsor-cli narsor-web narsor-server narsor-controller narsor-runner narsor-observer-worker narsor-migrate)
bashrc_marker="# narsor-installer"

fail() {
    printf 'narsor installer: %s\n' "$*" >&2
    exit 1
}

# Nagłówek kroku: pusta linia i tytuł, pogrubiony na terminalu, żeby kroki
# instalacji (i pytania `narsor-cli init`) nie zlewały się w jeden blok.
step() {
    if [[ -t 1 ]]; then
        printf '\n\033[1m==> %s\033[0m\n' "$*"
    else
        printf '\n==> %s\n' "$*"
    fi
}

# Wersja z manifestu archiwum (pole "version"); pusta dla starszych wydań.
manifest_version() {
    sed -n 's/^  "version": "\(.*\)",$/\1/p' "$1"
}

usage() {
    cat <<'USAGE'
Instalacja:
  install.sh [wersja] [--role local|frontend|server] [--bin-dir KATALOG]
             [--with-frontend] [--update-bashrc] [--non-interactive]
             [--stop-running] [--migrate] [--progress]
             [opcje narsor-cli init, np. --data-dir, --config-file]
             (init options include --local-mode process|docker for the local role)
             (--migrate: with --non-interactive, move the data of an older
              version with narsor-migrate --yes; see docs/operations/updates.md)
             (an old runtime catalogue moves to {catalog}/runtime/ on every
              install or update; see docs/operations/updates.md)
Odinstalowanie:
  install.sh --uninstall [--purge] [--bin-dir KATALOG]
             [--data-dir KATALOG] [--config-file PLIK] [--non-interactive]
USAGE
}

shell_quote() {
    local quoted=${1//\'/\'\\\'\'}
    printf "'%s'" "$quoted"
}

absolute() {
    case "$1" in
        /*) printf '%s\n' "$1" ;;
        *) printf '%s/%s\n' "$PWD" "$1" ;;
    esac
}

# Terminal użytkownika. Przy `curl ... | bash` standardowe wejście to potok ze
# skryptem, więc pytania muszą czytać z /dev/tty.
tty_available() {
    { : </dev/tty; } 2>/dev/null
}

ask() {
    local answer=
    printf '%s' "$1" >&2
    IFS= read -r answer </dev/tty || true
    printf '%s\n' "$answer"
}

# Menu sterowany strzałkami (lub j/k) i Enter.
select_role() {
    local -a keys=(local frontend server)
    local -a labels=(
        "Local     TUI, CLI, server and controller on this machine"
        "Frontend  TUI and CLI for a separately operated server"
        "Server    network server, controller and runner"
    )
    local index=0 key rest position count=${#keys[@]}
    printf 'Role (↑/↓ to move, Enter to confirm)\n' >&2
    while true; do
        for position in "${!labels[@]}"; do
            if ((position == index)); then
                printf '  \033[7m› %s \033[0m\n' "${labels[position]}" >&2
            else
                printf '    %s\n' "${labels[position]}" >&2
            fi
        done
        IFS= read -rsn1 key </dev/tty || fail "cannot read from the terminal"
        if [[ "$key" == $'\033' ]]; then
            rest=
            IFS= read -rsn2 -t 0.1 rest </dev/tty || true
            case "$rest" in
                '[A') key=k ;;
                '[B') key=j ;;
                *) key=none ;;
            esac
        fi
        case "$key" in
            k) index=$(((index + count - 1) % count)) ;;
            j) index=$(((index + 1) % count)) ;;
            '') break ;;
        esac
        printf '\033[%dA\033[J' "$count" >&2
    done
    printf '\033[%dA\033[J' "$((count + 1))" >&2
    role=${keys[index]}
    printf 'Role: %s\n' "$role" >&2
}

# Pole z podpowiedzią (edytowalną) i uzupełnianiem ścieżek klawiszem Tab.
ask_path() {
    local answer=
    IFS= read -r -e -i "$2" -p "$1: " answer </dev/tty || true
    printf '%s\n' "$answer"
}

binaries_for_role() {
    case "$role" in
        local) printf '%s\n' "${all_binaries[@]}" ;;
        frontend) printf '%s\n' narsor narsor-cli narsor-web ;;
        server)
            if ((with_frontend)); then
                printf '%s\n' "${all_binaries[@]}"
            else
                printf '%s\n' narsor-server narsor-controller narsor-runner narsor-observer-worker narsor-migrate
            fi
            ;;
    esac
}

path_contains() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

bashrc_line() {
    printf 'export PATH="$PATH":%s %s' "$(shell_quote "$1")" "$bashrc_marker"
}

add_to_bashrc() {
    local bashrc="$HOME/.bashrc" line
    line="$(bashrc_line "$1")"
    if [[ -f "$bashrc" ]] && grep -qxF -- "$line" "$bashrc"; then
        return
    fi
    if [[ -s "$bashrc" && -n "$(tail -c1 "$bashrc")" ]]; then
        printf '\n' >>"$bashrc"
    fi
    printf '%s\n' "$line" >>"$bashrc"
    printf "Added '%s' to %s.\n" "$1" "$bashrc"
}

remove_from_bashrc() {
    local bashrc="$HOME/.bashrc" line temp
    line="$(bashrc_line "$1")"
    [[ -f "$bashrc" ]] && grep -qxF -- "$line" "$bashrc" || return 0
    temp="$(mktemp)"
    grep -vxF -- "$line" "$bashrc" >"$temp" || true
    # Zapis do istniejącego pliku zachowuje jego uprawnienia i dowiązanie.
    cat "$temp" >"$bashrc"
    rm -f "$temp"
    printf "Removed '%s' from %s.\n" "$1" "$bashrc"
}

# Procesy Narsora tego użytkownika: plik wykonywalny (/proc/PID/exe) nazywa się
# jak jedna z naszych binariów. Nazwa z `comm` się nie nadaje, bo jądro obcina
# ją do 15 znaków (narsor-controller ma 16). Wypisuje wiersze "PID<TAB>ścieżka".
# Pomija sam instalator i jego rodzica.
running_narsor_processes() {
    local exe pid path name known
    for exe in /proc/[0-9]*/exe; do
        pid=${exe#/proc/}
        pid=${pid%/exe}
        ((pid == $$ || pid == PPID)) && continue
        path="$(readlink "$exe" 2>/dev/null)" || continue
        path=${path% (deleted)}
        name=${path##*/}
        for known in "${all_binaries[@]}"; do
            if [[ "$name" == "$known" ]]; then
                printf '%s\t%s\n' "$pid" "$path"
                break
            fi
        done
    done
}

# SIGTERM, do 10 s czekania, potem SIGKILL dla tych, które nie wyszły.
stop_processes() {
    local pid waited=0
    local -a alive=("$@")
    kill -TERM "${alive[@]}" 2>/dev/null || true
    while ((waited < 10)); do
        local -a still=()
        for pid in "${alive[@]}"; do
            kill -0 "$pid" 2>/dev/null && still+=("$pid")
        done
        alive=(${still[@]+"${still[@]}"})
        ((${#alive[@]} == 0)) && return 0
        sleep 1
        waited=$((waited + 1))
    done
    printf 'Still running after 10 s, sending SIGKILL: %s\n' "${alive[*]}" >&2
    kill -KILL "${alive[@]}" 2>/dev/null || true
}

# Wymiana binariów nie przerywa działających procesów (zachowują stary plik do
# końca), ale pracują na starej wersji. Pytamy więc, czy je zatrzymać.
handle_running_processes() {
    local line pid path answer
    local -a lines=() pids=()
    mapfile -t lines < <(running_narsor_processes)
    ((${#lines[@]})) || return 0
    printf 'Narsor is running (%d process(es)):\n' "${#lines[@]}"
    for line in "${lines[@]}"; do
        pid=${line%%$'\t'*}
        path=${line#*$'\t'}
        pids+=("$pid")
        printf '  %s  %s\n' "$pid" "$path"
    done
    if ((!stop_running)); then
        if ((interactive)); then
            answer="$(ask 'Stop them before installing? Running tasks are interrupted. [y/N]: ')"
            case "${answer,,}" in y|yes) stop_running=1 ;; esac
        else
            printf 'Not stopping them; pass --stop-running to do so.\n'
        fi
    fi
    if ((stop_running)); then
        stop_processes "${pids[@]}"
        printf 'Stopped %d process(es).\n' "${#pids[@]}"
    else
        narsor_running=1
        printf 'They keep running the previous version until you restart them.\n'
    fi
}

# Loud: a step the operator must not miss.
warn_loudly() {
    if [[ -t 1 ]]; then
        printf '\033[1;33m!!! %s\033[0m\n' "$*"
    else
        printf '!!! %s\n' "$*"
    fi
}

# The configuration narsor-server reads: an explicit --config/--config-file
# given to the installer, else the one the cascade finds (no flag).
migrate_scope_args() {
    local -a args=() given=("${passthrough[@]}")
    local index=0
    while ((index < ${#given[@]})); do
        case "${given[index]}" in
            --config|--config-file)
                ((index + 1 < ${#given[@]})) && args=(--config "${given[index + 1]}")
                index=$((index + 2))
                continue
                ;;
            --config=*|--config-file=*) args=(--config "${given[index]#*=}") ;;
        esac
        index=$((index + 1))
    done
    if ((${#args[@]})); then
        printf '%s\n' "${args[@]}"
    fi
}

# Moves the data of an older version to the per-binary layout with
# narsor-migrate (backup first). Only for roles that keep server or local
# controller data; a no-op when there is nothing to migrate.
run_migration() {
    local migrate="$bin_dir/narsor-migrate" output status=0 command arg
    local -a scope
    case "$role" in local|server) ;; *) return 0 ;; esac
    [[ -x "$migrate" ]] || return 0
    step "Data migration"
    mapfile -t scope < <(migrate_scope_args)
    command="$(shell_quote "$migrate")"
    for arg in ${scope[@]+"${scope[@]}"}; do
        command+=" $(shell_quote "$arg")"
    done
    output="$("$migrate" ${scope[@]+"${scope[@]}"} --dry-run 2>&1)" || status=$?
    if ((status == 0)) && [[ "$output" == "Nothing to migrate." ]]; then
        printf 'No data of an older version to migrate.\n'
        return 0
    fi
    if ((status == 2)); then
        printf '%s\n' "$output" >&2
        warn_loudly "narsor-migrate cannot read the configuration; correct it, then run: $command"
        return 0
    fi
    if ((narsor_running)); then
        printf '%s\n' "$output"
        warn_loudly "The data of an older version was NOT migrated: Narsor is still running."
        warn_loudly "The new narsor-server refuses to start until it is. Stop every Narsor process (narsor-server stop), then run: $command"
        return 0
    fi
    status=0
    if ((interactive)); then
        "$migrate" ${scope[@]+"${scope[@]}"} </dev/tty || status=$?
    elif ((run_migrate)); then
        "$migrate" ${scope[@]+"${scope[@]}"} --yes || status=$?
    else
        printf '%s\n' "$output"
        warn_loudly "The data of an older version was NOT migrated (no --migrate). The new narsor-server refuses to start until it is. Run: $command --yes"
        return 0
    fi
    if ((status != 0)); then
        warn_loudly "narsor-migrate stopped (exit status $status); the new narsor-server refuses to start until the data is migrated. See the message above, then run: $command"
        ((interactive)) || fail "the data migration did not complete"
    fi
}

# Moves an old runtime catalogue ({catalog}/runtime.yaml and components/) into
# {catalog}/runtime/ with narsor-migrate runtime-catalog. It only renames, so
# there is no backup and no question; it needs Narsor stopped. Only for the
# roles whose configuration names the catalogue the server reads.
move_runtime_catalog() {
    local migrate="$bin_dir/narsor-migrate" status=0 command arg answer line output
    local -a scope lines=() pids=()
    case "$role" in local|server) ;; *) return 0 ;; esac
    [[ -x "$migrate" ]] || return 0
    step "Runtime catalogue"
    mapfile -t scope < <(migrate_scope_args)
    command="$(shell_quote "$migrate")"
    for arg in ${scope[@]+"${scope[@]}"}; do
        command+=" $(shell_quote "$arg")"
    done
    command+=" runtime-catalog"
    # Probe first: with nothing to move, neither a question nor a warning.
    output="$("$migrate" ${scope[@]+"${scope[@]}"} runtime-catalog --dry-run 2>&1)" || status=$?
    if ((status == 0)) && [[ "$output" == "No runtime catalogue at the old place"* ]]; then
        printf 'No old runtime catalogue to move.\n'
        return 0
    fi
    status=0
    if ((narsor_running && interactive)); then
        answer="$(ask 'Narsor still runs. Stop it now to move an old runtime catalogue? Running tasks are interrupted. [y/N]: ')"
        case "${answer,,}" in
            y|yes)
                mapfile -t lines < <(running_narsor_processes)
                for line in ${lines[@]+"${lines[@]}"}; do
                    pids+=("${line%%$'\t'*}")
                done
                if ((${#pids[@]})); then
                    stop_processes "${pids[@]}"
                    printf 'Stopped %d process(es).\n' "${#pids[@]}"
                fi
                narsor_running=0
                ;;
        esac
    fi
    if ((narsor_running)); then
        warn_loudly "An old runtime catalogue was NOT moved to {catalog}/runtime/: Narsor is still running. Tasks keep using the old place until it moves. Stop every Narsor process, then run: $command"
        return 0
    fi
    "$migrate" ${scope[@]+"${scope[@]}"} runtime-catalog || status=$?
    case "$status" in
        0) ;;
        5)
            warn_loudly "An old runtime catalogue was NOT moved to {catalog}/runtime/: a Narsor program still runs. Tasks keep using the old place until it moves. Stop every Narsor process, then run: $command"
            ;;
        2)
            warn_loudly "narsor-migrate cannot read the configuration, so an old runtime catalogue was not moved; correct it, then run: $command"
            ;;
        *)
            warn_loudly "The runtime catalogue was NOT moved (exit status $status). See the message above, correct it, then run: $command"
            ((interactive)) || fail "the runtime catalogue move did not complete"
            ;;
    esac
}

install_binaries() {
    local source_dir=$1 destination=$2 name
    shift 2
    mkdir -p "$destination" || fail "cannot create '$destination'"
    for name in "$@"; do
        cp "$source_dir/$name" "$destination/.$name.new.$$" \
            && chmod 755 "$destination/.$name.new.$$" \
            && mv -f "$destination/.$name.new.$$" "$destination/$name" \
            || { rm -f "$destination/.$name.new.$$"; fail "cannot install '$name' to '$destination'"; }
    done
}

uninstall() {
    local name cli
    step "Uninstalling Narsor"
    if ((purge)); then
        cli="$bin_dir/narsor-cli"
        [[ -x "$cli" ]] || fail "narsor-cli is not in '$bin_dir'; it is needed for --purge (use --bin-dir)"
        local purge_args=(purge)
        ((non_interactive)) && purge_args+=(--yes)
        if ((interactive)); then
            "$cli" "${purge_args[@]}" "${passthrough[@]}" </dev/tty
        else
            "$cli" "${purge_args[@]}" "${passthrough[@]}"
        fi
    fi
    for name in "${all_binaries[@]}"; do
        rm -f -- "$bin_dir/$name"
    done
    remove_from_bashrc "$bin_dir"
    printf "Removed Narsor binaries from '%s'.\n" "$bin_dir"
    if ((!purge)); then
        printf 'Configuration and data were kept; add --purge to remove them.\n'
    fi
}

install() {
    local download work_dir entry binary
    [[ "$(uname -s)" == "Linux" ]] || fail "this installer supports Linux only"
    [[ "$(uname -m)" == "x86_64" ]] || fail "this release supports x86_64 only"
    command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required"

    local base_url
    if [[ "$version" != "latest" ]]; then
        [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must be latest or vMAJOR.MINOR.PATCH"
        base_url="https://github.com/$repository/releases/download/$version"
    else
        base_url="https://github.com/$repository/releases/latest/download"
    fi

    if command -v curl >/dev/null 2>&1; then
        # Trzeci argument "progress": pasek postępu zamiast trybu cichego.
        download() {
            local quiet=(--silent --show-error)
            [[ "${3:-}" == progress ]] && quiet=(--progress-bar)
            curl --fail --location --proto '=https' --proto-redir '=https' "${quiet[@]}" \
                "$1" --output "$2"
        }
    elif command -v wget >/dev/null 2>&1; then
        download() {
            local quiet=(--quiet)
            [[ "${3:-}" == progress ]] && quiet=(--quiet --show-progress)
            wget --https-only "${quiet[@]}" "$1" --output-document="$2"
        }
    else
        fail "curl or wget is required"
    fi

    work_dir="$(mktemp -d)"
    # Zmienna lokalna znika przed EXIT, więc ścieżka jest wstawiana od razu.
    trap "rm -rf -- $(shell_quote "$work_dir")" EXIT
    step "Downloading Narsor ($version)"
    download "$base_url/$archive" "$work_dir/$archive" "$( ((show_progress)) && printf progress)"
    download "$base_url/$archive.sha256" "$work_dir/$archive.sha256"

    step "Verifying the archive"
    (cd "$work_dir" && sha256sum --check "$archive.sha256") || fail "release archive checksum verification failed"

    while IFS= read -r entry; do
        case "$entry" in
            manifest.json|narsor|narsor-cli|narsor-web|narsor-server|narsor-controller|narsor-runner|narsor-observer-worker|narsor-migrate) ;;
            *) fail "unexpected path in release archive: $entry" ;;
        esac
    done < <(tar -tzf "$work_dir/$archive")

    tar -xzf "$work_dir/$archive" -C "$work_dir"
    [[ -f "$work_dir/manifest.json" && ! -L "$work_dir/manifest.json" ]] || fail "release archive has no regular manifest"
    grep -Fq "\"target\": \"$target\"" "$work_dir/manifest.json" || fail "release archive target does not match $target"
    for binary in "${all_binaries[@]}"; do
        [[ -f "$work_dir/$binary" && ! -L "$work_dir/$binary" && -x "$work_dir/$binary" ]] \
            || fail "release archive is missing a regular executable: $binary"
    done
    local release_version
    release_version="$(manifest_version "$work_dir/manifest.json")"
    if [[ "$version" != "latest" && -n "$release_version" && "v$release_version" != "$version" ]]; then
        fail "release archive is version $release_version, not $version"
    fi
    printf 'Narsor %s, %s: archive verified.\n' "${release_version:-?}" "$target"

    step "Role"
    if [[ -z "$role" ]]; then
        ((interactive)) || fail "non-interactive installation requires --role local, frontend, or server"
        select_role
    else
        printf 'Role: %s\n' "$role"
    fi
    case "$role" in
        local|frontend|server) ;;
        *) fail "role must be local, frontend, or server" ;;
    esac
    step "Binary directory"
    if [[ -z "$bin_dir" ]]; then
        ((interactive)) || fail "non-interactive installation requires --bin-dir"
        bin_dir="$(ask_path 'Binary installation directory (Tab completes, Enter accepts)' "$HOME/.local/bin")"
        bin_dir="${bin_dir:-$HOME/.local/bin}"
        case "$bin_dir" in
            '~') bin_dir=$HOME ;;
            '~/'*) bin_dir="$HOME/${bin_dir#\~/}" ;;
        esac
    fi
    bin_dir="$(absolute "$bin_dir")"
    printf 'Binaries: %s\n' "$bin_dir"

    # Konfiguracja najpierw: błąd walidacji nie zostawia po sobie binariów.
    step "Configuration"
    local init_args=(init --role "$role")
    ((non_interactive)) && init_args+=(--non-interactive)
    if ((interactive)); then
        "$work_dir/narsor-cli" "${init_args[@]}" "${passthrough[@]}" </dev/tty
    else
        "$work_dir/narsor-cli" "${init_args[@]}" "${passthrough[@]}"
    fi

    step "Installing binaries"
    local -a binaries
    mapfile -t binaries < <(binaries_for_role)
    handle_running_processes
    install_binaries "$work_dir" "$bin_dir" "${binaries[@]}"
    printf 'Installed: %s\n' "${binaries[*]}"
    run_migration
    move_runtime_catalog

    if ! path_contains "$bin_dir"; then
        step "PATH"
        local consent=0 answer
        if ((update_bashrc)); then
            consent=1
        elif ((interactive)); then
            answer="$(ask "'$bin_dir' is not on PATH. Add it to ~/.bashrc? [y/N]: ")"
            case "${answer,,}" in y|yes) consent=1 ;; esac
        fi
        if ((consent)); then
            add_to_bashrc "$bin_dir"
        else
            printf "Add '%s' to PATH in your shell configuration to run Narsor by name.\n" "$bin_dir"
        fi
    fi
    step "Done"
    printf "Installed Narsor %s (%s role) in '%s'.\n" "${release_version:-$version}" "$role" "$bin_dir"
}

main() {
    local role= bin_dir= with_frontend=0 update_bashrc=0 non_interactive=0 stop_running=0 show_progress=0
    local uninstall_mode=0 purge=0 interactive=0 run_migrate=0 narsor_running=0
    local -a passthrough=()

    if [[ "${1:-}" == "latest" || "${1:-}" == v* ]]; then
        version="$1"
        shift
    fi
    while (($#)); do
        case "$1" in
            --role|--bin-dir)
                (($# >= 2)) || fail "$1 needs a value"
                case "$1" in --role) role=$2 ;; *) bin_dir=$2 ;; esac
                shift 2
                ;;
            --role=*) role=${1#*=}; shift ;;
            --bin-dir=*) bin_dir=${1#*=}; shift ;;
            --with-frontend) with_frontend=1; shift ;;
            --update-bashrc) update_bashrc=1; shift ;;
            --stop-running) stop_running=1; shift ;;
            --migrate) run_migrate=1; shift ;;
            --progress) show_progress=1; shift ;;
            --non-interactive) non_interactive=1; shift ;;
            --uninstall) uninstall_mode=1; shift ;;
            --purge) purge=1; shift ;;
            -h|--help) usage; return 0 ;;
            *) passthrough+=("$1"); shift ;;
        esac
    done
    ((!purge || uninstall_mode)) || fail "--purge works only with --uninstall"

    if ((!non_interactive)) && tty_available; then
        interactive=1
    fi

    if ((uninstall_mode)); then
        bin_dir="$(absolute "${bin_dir:-$HOME/.local/bin}")"
        uninstall
    else
        install
    fi
}

# Cały skrypt jest wczytany przed uruchomieniem; `exit` nie pozwala bashowi
# czytać dalej z potoku, gdy skrypt przyszedł przez `curl | bash`.
main "$@"; exit $?
