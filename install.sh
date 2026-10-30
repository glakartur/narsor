#!/usr/bin/env bash
# Instaluje wszystkie binaria Narsor (narsor, narsor-cli, narsor-server,
# narsor-runner, narsor-controller) do jednego katalogu, tak aby klient
# znalazł serwer, a serwer lokalny runner.
#
# Potem kopiuje workflow z katalogu workflows/ do {catalog_dir}/workflows.
# Gdy plik konfiguracji ($XDG_CONFIG_HOME/narsor/config.yaml, domyślnie
# ~/.config/narsor/config.yaml) wskazuje catalog_dir, używa go. Gdy pliku nie
# ma, pyta o katalog główny Narsor (whiptail, dialog lub zwykły prompt) i
# zapisuje catalog_dir, state_dir i work_dir. Istniejącego pliku konfiguracji
# nigdy nie zmienia.
#
# Użycie: ./install.sh [--root KATALOG] [--skip-workflows]
#   --root KATALOG     katalog instalacji (binaria trafią do KATALOG/bin);
#                      domyślnie katalog cargo (zwykle ~/.cargo).
#   --skip-workflows   nie kopiuj workflow i nie pytaj o konfigurację.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$REPO_ROOT"

root_args=()
skip_workflows=0
while (($#)); do
    case "$1" in
        --root)
            [[ $# -ge 2 ]] || { echo 'Brak wartości dla --root.' >&2; exit 2; }
            root_args=(--root "$2")
            shift 2
            ;;
        --skip-workflows)
            skip_workflows=1
            shift
            ;;
        -h|--help)
            sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Nieznana opcja: $1 (zobacz --help)" >&2
            exit 2
            ;;
    esac
done

# --- Workflow i konfiguracja -------------------------------------------------

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/narsor"

# Pierwszy istniejący plik konfiguracji (tak samo jak narsor: .yaml, potem .yml).
find_config_file() {
    local candidate
    for candidate in "$config_dir/config.yaml" "$config_dir/config.yml"; do
        if [[ -e $candidate || -L $candidate ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

# Wartość klucza najwyższego poziomu z prostego pliku YAML: bez komentarza,
# cudzysłowów i spacji, z rozwiniętym początkowym '~'.
read_config_value() {
    local key="$1" file="$2" value
    value="$(sed -n "s/^${key}:[[:space:]]*//p" "$file" | head -n 1)"
    value="$(printf '%s' "$value" | sed -e 's/[[:space:]]#.*$//' -e 's/[[:space:]]*$//' -e "s/^\"\(.*\)\"\$/\1/" -e "s/^'\(.*\)'\$/\1/")"
    case "$value" in
        '~') value="$HOME" ;;
        '~/'*) value="$HOME/${value#\~/}" ;;
    esac
    printf '%s' "$value"
}

# Pyta o katalog główny Narsor. Wynik na stdout, kod 1 przy anulowaniu lub gdy
# nie ma terminala.
ask_narsor_root() {
    local default='~/narsor' prompt answer
    prompt='Katalog główny Narsor. Powstaną w nim podkatalogi catalog, state i work.'
    [[ -t 0 && -t 2 ]] || return 1
    if command -v whiptail >/dev/null 2>&1; then
        whiptail --title 'Narsor' --inputbox "$prompt" 10 70 "$default" 3>&1 1>&2 2>&3
    elif command -v dialog >/dev/null 2>&1; then
        dialog --title 'Narsor' --stdout --inputbox "$prompt" 10 70 "$default"
    else
        read -r -p "$prompt [$default]: " answer || return 1
        printf '%s\n' "${answer:-$default}"
    fi
}

# Zapisuje nowy plik konfiguracji. Wpisy zachowują '~', bo narsor rozwija go sam.
write_new_config() {
    local root="${1%/}" file="$config_dir/config.yaml"
    mkdir -p "$config_dir"
    {
        printf 'catalog_dir: %s/catalog\n' "$root"
        printf 'state_dir: %s/state\n' "$root"
        printf 'work_dir: %s/work\n' "$root"
    } > "$file"
    echo "Zapisano konfigurację: $file" >&2
}

# Kopiuje workflows/*.yaml do {catalog_dir}/workflows.
copy_workflows() {
    local catalog="$1" target file name
    target="$catalog/workflows"
    mkdir -p "$target"
    for file in "$REPO_ROOT"/workflows/*.yaml; do
        name="$(basename "$file")"
        if [[ -f $target/$name ]] && cmp -s "$file" "$target/$name"; then
            continue
        fi
        cp -f "$file" "$target/$name"
        echo "Skopiowano workflow: $name" >&2
    done
    echo "Workflow są w $target." >&2
}

install_workflows() {
    local config_file catalog root
    if config_file="$(find_config_file)"; then
        catalog="$(read_config_value catalog_dir "$config_file")"
        if [[ -z $catalog ]]; then
            echo "Uwaga: $config_file nie ustawia catalog_dir; pomijam kopiowanie workflow." >&2
            echo "Dodaj w nim catalog_dir i uruchom install.sh ponownie. Pliku nie zmieniam." >&2
            return 0
        fi
    else
        if ! root="$(ask_narsor_root)" || [[ -z $root ]]; then
            echo "Uwaga: brak konfiguracji i brak odpowiedzi; pomijam kopiowanie workflow." >&2
            return 0
        fi
        write_new_config "$root"
        catalog="$(read_config_value catalog_dir "$config_dir/config.yaml")"
    fi
    case "$catalog" in
        /*) ;;
        *) echo "Uwaga: catalog_dir '$catalog' nie jest ścieżką bezwzględną; pomijam kopiowanie workflow." >&2
           return 0 ;;
    esac
    copy_workflows "$catalog"
}
# --- Koniec: workflow i konfiguracja -----------------------------------------

# Wymagane do budowy i działania.
for tool in cargo git; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Brak '$tool' na PATH. Zainstaluj go i uruchom skrypt ponownie." >&2
        exit 1
    fi
done

# Potrzebne tylko niektórym workflow; ich brak wychodzi dopiero przy uruchomieniu kroku.
if ! command -v claude >/dev/null 2>&1 && ! command -v opencode >/dev/null 2>&1; then
    echo "Uwaga: brak harnessu agenta ('claude' lub 'opencode') na PATH. Kroki agentowe nie ruszą." >&2
fi
for package in . crates/narsor-server crates/narsor-runner crates/narsor-controller; do
    echo "Instaluję $package ..."
    cargo install --locked --force --path "$package" --bins "${root_args[@]}"
done

if ((${#root_args[@]})); then
    bin_dir="${root_args[1]}/bin"
else
    bin_dir="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
fi

echo "Zainstalowano binaria Narsor w $bin_dir."

if ((skip_workflows == 0)); then
    install_workflows
fi

# Działający serwer to nadal stara binarka: tworzyłby snapshoty workflow w
# starym schemacie, a nowy runner by je odrzucał. Zatrzymujemy go; klient
# uruchomi nowy serwer przy następnym wywołaniu. Runnerów działających zadań
# to nie zatrzymuje (mają własną grupę procesów).
if "$bin_dir/narsor-server" stop >/dev/null 2>&1; then
    echo "Zatrzymano działający serwer Narsor; klient uruchomi nowy przy następnym wywołaniu."
    echo "Zamknij i uruchom ponownie otwarte okna 'narsor', bo działają na starej binarce."
fi
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "Dodaj $bin_dir do PATH, np.: export PATH=\"$bin_dir:\$PATH\"" ;;
esac
