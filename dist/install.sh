#!/usr/bin/env bash
# Instalator wydania Narsor dla Linux x86_64.
#
# Instalacja:    install.sh [wersja] [--role local|frontend|server] [--bin-dir KATALOG]
#                           [--with-frontend] [--update-bashrc] [--non-interactive]
#                           [opcje narsor-cli init, np. --data-dir, --config-file]
# Odinstalowanie: install.sh --uninstall [--purge] [--bin-dir KATALOG]
#                           [--data-dir KATALOG] [--config-file PLIK] [--non-interactive]
#
# Skrypt pobiera i weryfikuje archiwum, kopiuje binaria i (za zgodą) dopisuje
# katalog do ~/.bashrc. Konfigurację tworzy `narsor-cli init`: pyta o katalog
# główny danych (domyślnie ~/.narsor; w pliku trafiają catalog_dir, state_dir
# i work_dir). Istniejącej konfiguracji nie rusza: obok zapisuje config.yaml.new.
set -euo pipefail

repository="glakartur/narsor"
target="x86_64-unknown-linux-gnu"
archive="narsor-linux-x86_64-gnu.tar.gz"
version="${NARSOR_VERSION:-latest}"
all_binaries=(narsor narsor-cli narsor-server narsor-controller narsor-runner)
bashrc_marker="# narsor-installer"

fail() {
    printf 'narsor installer: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'USAGE'
Instalacja:
  install.sh [wersja] [--role local|frontend|server] [--bin-dir KATALOG]
             [--with-frontend] [--update-bashrc] [--non-interactive]
             [opcje narsor-cli init, np. --data-dir, --config-file]
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
        frontend) printf '%s\n' narsor narsor-cli ;;
        server)
            if ((with_frontend)); then
                printf '%s\n' "${all_binaries[@]}"
            else
                printf '%s\n' narsor-server narsor-controller narsor-runner
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
    # Zmienna lokalna znika przed EXIT, więc ścieżka jest wstawiana od razu.
    trap "rm -rf -- $(shell_quote "$work_dir")" EXIT
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
    for binary in "${all_binaries[@]}"; do
        [[ -f "$work_dir/$binary" && ! -L "$work_dir/$binary" && -x "$work_dir/$binary" ]] \
            || fail "release archive is missing a regular executable: $binary"
    done

    if [[ -z "$role" ]]; then
        ((interactive)) || fail "non-interactive installation requires --role local, frontend, or server"
        select_role
    fi
    case "$role" in
        local|frontend|server) ;;
        *) fail "role must be local, frontend, or server" ;;
    esac
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

    # Konfiguracja najpierw: błąd walidacji nie zostawia po sobie binariów.
    local init_args=(init --role "$role")
    ((non_interactive)) && init_args+=(--non-interactive)
    if ((interactive)); then
        "$work_dir/narsor-cli" "${init_args[@]}" "${passthrough[@]}" </dev/tty
    else
        "$work_dir/narsor-cli" "${init_args[@]}" "${passthrough[@]}"
    fi

    local -a binaries
    mapfile -t binaries < <(binaries_for_role)
    install_binaries "$work_dir" "$bin_dir" "${binaries[@]}"

    if ! path_contains "$bin_dir"; then
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
    printf "Installed the %s role in '%s'.\n" "$role" "$bin_dir"
}

main() {
    local role= bin_dir= with_frontend=0 update_bashrc=0 non_interactive=0
    local uninstall_mode=0 purge=0 interactive=0
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
