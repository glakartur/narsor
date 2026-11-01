---
title: Ograniczenie rozmiaru folderu target - Plan
type: chore
date: 2026-11-01
topic: rozmiar-folderu-target
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
product_contract_source: docs/brainstorms/2026-11-01-rozmiar-folderu-target-requirements.md
execution: code
---

# Ograniczenie rozmiaru folderu target - Plan

## Goal Capsule

- **Objective:** Zmniejszyć rozmiar `target/` (dziś ok. 35 GB, głównie dev/test) lokalnie i w CI bez zmiany zachowania produkcyjnych binarek.
- **Means:** (1) pomiar, (2) profile Cargo obniżające debug info zależności i strip w release, (3) CI: `CARGO_INCREMENTAL=0` i weryfikacja cache, (4) scalenie testów integracyjnych w jeden crate testowy.
- **Authority:** Dokument wymagań (R1–R4, rozstrzygnięte pytania). Wspólny `CARGO_TARGET_DIR` jest odrzucony. Własny kod workspace'u zachowuje pełne debug info. C1 jest zatwierdzone.
- **Stop conditions:** R1–R4 spełnione; `cargo test --locked --workspace` i `cargo build --workspace --bins --release` przechodzą; zmierzony rozmiar przed/po zapisany.

## Requirements Trace

- R1 → U1 (pomiar przed i po, zapis w sekcji Wyniki tego planu).
- R2 → U2 (bez zmiany `opt-level`/`panic`; tylko debug/strip/split-debuginfo).
- R3 → U2, U3, U4 (każda jednostka kończy się uruchomieniem obu poleceń).
- R4 → U3, U4 (porównanie czasu czystego builda CI przed/po).

## Scope Boundaries

- Poza zakresem: wspólny `CARGO_TARGET_DIR`, `sccache`, zmiana linkera (G1), kompresja FS (G2), zmiana `opt-level`/`panic`.
- Poza zakresem: D1–D4 (flagi `run.sh`, features `tokio`, `cargo tree -d`) – osobny follow-up, jeśli pomiar po U2–U4 nadal pokaże potrzebę.
- Założenie do potwierdzenia przy U2: rozmiar artefaktów w `dist/` nie jest celem, ale `strip` (B4) musi być sprawdzony wobec `dist/package-linux.sh` i `tests/release_artifacts.rs`.

## Context

- `Cargo.toml` (root, package `narsor` + workspace, 9 członków) nie ma sekcji `[profile.*]`; brak `.cargo/config.toml`.
- `tests/` zawiera ok. 64 pliki `*.rs` z testami integracyjnymi (każdy = osobny binarny crate testowy) oraz katalogi pomocnicze: `backend_contract_support/`, `engine_support/`, `environment_support/`, `kubernetes_support/`, `remote_support/`, `task_app_support/`, `fixtures/`. `crates/narsor-controller/tests/cli.rs` to osobny, pojedynczy test.
- Testy używają `env!("CARGO_MANIFEST_DIR")` + `tests/fixtures/...` (ścieżki od roota crate'u – niezależne od układu modułów) oraz `env!("CARGO_BIN_EXE_narsor-cli")` i `env!("CARGO")` (dostępne w każdym crate testowym pakietu `narsor`).
- CI: `.github/workflows/ci.yml` i `release.yml` uruchamiają `cargo test --locked --workspace` z `actions-rust-lang/setup-rust-toolchain` (domyślnie włącza cache `Swatinem/rust-cache`).
- `run.sh` buduje `--workspace --bins --release`.

## Implementation Units

### U1. Pomiar bazowy (R1)

- **Cel:** Ustalić, co dominuje w `target/`.
- **Kroki:** `du -sh target/*`, `du -sh target/debug/{deps,incremental,build}`, liczba i rozmiar największych plików w `target/debug/deps` (`ls -S | head`). Zapisać bazową sumę `passed` z `cargo test --workspace` (używana w U4). Na końcu zmierzyć czas `cargo clean && cargo test --workspace --no-run` (baza do R4; `du` robić przed `clean`).
- **Wynik:** Tabela „przed” dopisana do sekcji Wyniki poniżej. Jeśli `incremental` lub `build` dominuje zamiast `deps`, zweryfikować kolejność U2–U4 (patrz Ryzyka).
- **Weryfikacja:** tabela wypełniona.

### U2. Profile Cargo (B1, B2, B4)

- **Pliki:** `Cargo.toml` (root workspace).
- **Zmiany:**
  - Sprawdzić, że toolchain (`rust-toolchain*`/CI) ma Rust ≥ 1.71 (wymagane dla `line-tables-only`).
  - `[profile.dev.package."*"] debug = "line-tables-only"` (zależności zewnętrzne; członkowie workspace'u nie są objęci `"*"`, więc zachowują pełne debug). Sprawdzić `cargo build -v`, że crate'y workspace'u nadal mają `-C debuginfo=2`.
  - `[profile.release] strip = "symbols"` – tylko po sprawdzeniu `dist/package-linux.sh` i `tests/release_artifacts.rs` (czy nie zależą od symboli); jeśli zależą, pominąć i odnotować.
  - B2 (`split-debuginfo = "unpacked"`) – dodać tylko jeśli toolchain Linux go wspiera i U1 pokaże zysk; przy braku efektu pominąć.
  - Nie dotykać `opt-level`, `panic`, `lto` (R2).
- **Test:** `cargo test --locked --workspace`, `cargo build --workspace --bins --release`; porównać rozmiar `target/debug` po czystym buildzie z U1.
- **Rollback:** usunięcie sekcji `[profile.*]`.

### U3. CI (E3, E4, R4)

- **Pliki:** `.github/workflows/ci.yml`, `.github/workflows/release.yml`.
- **Zmiany:**
  - Dodać `env: CARGO_INCREMENTAL: "0"` na poziomie jobu/workflow (sprawdzić, czy `setup-rust-toolchain` już go nie ustawia – ono domyślnie ustawia `CARGO_INCREMENTAL=0`; jeśli tak, nie duplikować i odnotować).
  - Zweryfikować w logach CI, że cache z `setup-rust-toolchain` działa i czyści stare wpisy; jeśli tak, E4 uznać za już spełnione, bez zmian.
- **Weryfikacja:** przebieg CI zielony; czas czystego builda nie wzrasta istotnie (R4).

### U4. Scalenie testów integracyjnych (C1)

- **Pliki:** `tests/*.rs` → `tests/integration/main.rs` + moduły; `Cargo.toml` (root) – ewentualnie `[[test]]`.
- **Podejście (etapowe, żeby diff był przeglądalny):**
  1. Utworzyć `tests/integration/main.rs` z `mod <nazwa>;` dla każdego scalanego pliku. Przenieść `tests/<nazwa>.rs` → `tests/integration/<nazwa>.rs` (`git mv`, zachowuje historię). Katalogi `*_support` przenieść do `tests/integration/` (lub zostawić i wskazać `#[path]`), tak by `mod xyz_support;` w modułach nadal się rozwiązywało – po przeniesieniu moduł `foo.rs` ma submoduły w `integration/foo/`, więc `mod engine_support;` zadeklarowany w `foo.rs` wymaga `#[path]` lub przeniesienia deklaracji do `main.rs` i zmiany na `crate::engine_support`. Wybrać jedną konwencję: deklaracje `mod *_support;` w `main.rs`, użycia jako `crate::*_support::...`.
  2. Naprawić: ścieżki względne `include_str!`/`include_bytes!`/`#[path]` (liczone od pliku źródłowego – po przeniesieniu o poziom głębiej wymagają korekty; `grep -rn 'include_\(str\|bytes\)!\|#\[path' tests`), inner doc comments `//!` (zostają w module, ok), `#![...]` atrybuty wewnętrzne (przenieść do `main.rs`), kolizje nazw globalnych (`static`, `#[test]` w tym samym module nie kolidują; kolizje dotyczą wspólnych helperów – są już w osobnych modułach).
  3. Testy zależne od infrastruktury (porty, singletony/stan globalny procesu, `set_var`/`set_current_dir` wymagające izolacji procesu) są **usuwane**, nie zabezpieczane mutexami ani zostawiane jako osobne crate'y testowe – nie utrzymujemy testów zależnych od infrastruktury. Wyszukać `set_var`/`set_current_dir`/globalne `static`/bindowanie portów, wypisać usuwane testy w opisie commita i w sekcji Wyniki. Pozostałe testy z różnych plików działają współbieżnie w jednym procesie – nie mogą współdzielić stanu globalnego.
  4. Testy uruchamiające dodatkowe procesy (podprocesy przez `CARGO_BIN_EXE_*`, wywołania `cargo`, np. `bin_targets.rs`, `auth.rs`, `cli_smoke.rs`) lub zależne od innych komponentów/procesów są **usuwane**. Przed usunięciem wypisać je i sprawdzić, czy nie pokrywają jedynego wymagania (`tests/release_artifacts.rs` – zob. U2); w razie wątpliwości zgłosić w PR zamiast usuwać po cichu. Jeśli po usunięciu plik nie ma już testów – usunąć plik.
  5. `crates/narsor-controller/tests/cli.rs` – pojedynczy plik, bez scalania.
  6. Dodać `autotests`-bezpieczeństwo: dzięki układowi `tests/integration/main.rs` Cargo wykrywa tylko jeden cel, więc nowe pliki muszą być dodane jako `mod` – odnotować w `docs/dev/` lub nagłówku `main.rs` (sprawdzić, czy istnieje dokument o konwencji testów i go uaktualnić).
- **Test:** `cargo test --locked --workspace` – suma `passed` po scaleniu równa bazowej pomniejszonej dokładnie o liczbę świadomie usuniętych testów (kroki 3–4; zapisać `cargo test --workspace 2>&1 | grep "test result"` przed i po). Uruchomić pakiet 3 razy, aby wykryć flaky wynikające z współbieżności.
- **Weryfikacja:** liczba plików wykonywalnych w `target/debug/deps` spada z ok. 65 do kilku; rozmiar i czas (R4) zmierzone.

### U5. Pomiar końcowy i dokumentacja

- Powtórzyć U1, dopisać tabelę „po” i wnioski. Jeśli cel nie osiągnięty, wskazać kolejne kroki z listy poza zakresem (D1–D4, E1 `cargo sweep`).
- Dodać krótką notkę o higienie (`cargo sweep --time 14`) do dokumentacji dev (`docs/dev/`), jeśli istnieje właściwy dokument.

## Kolejność i zależności

U1 → U2 → U3 (niezależne od U2, można równolegle) → U4 → U5. U4 to największy zysk, ale najwyższe ryzyko; wykonywać po U2, w osobnym commicie.

## Ryzyka

- Scalone testy działają współbieżnie w jednym procesie → flaky; mitygacja: testy zależne od stanu procesu/infrastruktury są usuwane (U4.3–4).
- `strip = "symbols"` może zmienić backtrace'y w binarkach release i zależności `dist/` → sprawdzić w U2.
- Czas kompilacji jednego dużego crate'u testowego bywa dłuższy przy zmianie jednego pliku (inkrementalnie), choć czysty build jest szybszy.
- Obniżone debug zależności pogarsza debugowanie w gdb/lldb (zaakceptowane).

## Wyniki (do uzupełnienia w U1/U5)

| Metryka | Przed | Po |
|---|---|---|
| `target/` razem (debug) | 30G | ok. 6G (bez release) |
| `target/debug/deps` | 26G | 5,2G |
| `target/debug/incremental` | 3,9G | 3,1G |
| `target/release` | nie mierzono | 2,6G (strip = symbols) |
| liczba plików wykonywalnych testów | 82 (bez rozszerzenia w `deps`) | 19 |
| czas czystego `cargo test --no-run` | nie mierzono osobno (czysty `cargo test`: 6m03) | 4m34 (po samych profilach: 4m56) |

Pośrednio po U2 (same profile): `target/debug` 23G, `deps` 19G.

Usunięte w U4 (zgodnie z U4.3–4; 249 passed, 1 failed, 17 ignored): pliki
uruchamiające podprocesy (`CARGO_BIN_EXE_*`, `Command::new`, `cargo`) –
`auth`, `bin_targets`, `checkpoint`, `cli_smoke`, `config_show`,
`config_startup`, `connection_profile`, `controller_cli`, `controller_local`,
`docs_install_url`, `engine_execution`, `installer`, `install_script`,
`materials`, `release_artifacts`, `takeover`, `tasks`, `workspace`.
Testy z efemerycznymi portami loopback (`:0`) zostały, bo nie dzielą stanu
procesu. Suma po scaleniu: 2660 passed (2909 − 249), 1 failed (istniejący
`conductor_invoke::claude_runs_read_only_...`), 3 uruchomienia bez flaky.
