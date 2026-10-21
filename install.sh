#!/usr/bin/env bash
# Instaluje wszystkie binaria Narsor (narsor, narsor-cli, narsor-server,
# narsor-runner, narsor-controller) do jednego katalogu, tak aby klient
# znalazł serwer, a serwer lokalny runner.
#
# Użycie: ./install.sh [--root KATALOG]
#   --root KATALOG  katalog instalacji (binaria trafią do KATALOG/bin);
#                   domyślnie katalog cargo (zwykle ~/.cargo).
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
cd "$REPO_ROOT"

root_args=()
while (($#)); do
    case "$1" in
        --root)
            [[ $# -ge 2 ]] || { echo 'Brak wartości dla --root.' >&2; exit 2; }
            root_args=(--root "$2")
            shift 2
            ;;
        -h|--help)
            sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Nieznana opcja: $1 (zobacz --help)" >&2
            exit 2
            ;;
    esac
done

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
if ! command -v glab >/dev/null 2>&1; then
    echo "Uwaga: brak 'glab' na PATH. Krok finalize workflow compound-engineering wybierze 'blocked'." >&2
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
