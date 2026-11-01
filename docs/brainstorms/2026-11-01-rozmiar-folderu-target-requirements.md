---
date: 2026-11-01
topic: rozmiar-folderu-target
---

# Ograniczenie rozmiaru folderu `target` (buildy)

## Summary

Folder `target/` rośnie, bo workspace (9 crate'ów, ok. 65 plików testów integracyjnych: 64 w `tests/` i 1 w `crates/narsor-controller/tests`) buduje się w profilach dev, test i release, a każdy test integracyjny to osobny, w pełni zlinkowany plik wykonywalny z symbolami debug. Poniżej opcje ograniczenia rozmiaru, ich koszt i rekomendowany pierwszy krok. Dokument nie wybiera ostatecznie – to lista możliwości do decyzji.

## Problem Frame

- W `Cargo.toml` nie ma żadnych sekcji `[profile.*]`, więc działają domyślne: pełne info debug w dev/test, brak optymalizacji linkowania.
- Nie ma `.cargo/config.toml` ani wspólnego `CARGO_TARGET_DIR`.
- `run.sh` buduje `--workspace --bins --release`, a CI (`.github/workflows/ci.yml`, `release.yml`) buduje osobno – artefakty release i dev rosną równolegle.
- `target/` jest już w `.gitignore`, więc problem dotyczy dysku (lokalnego, runnerów CI, obrazów kontenerów), nie repozytorium.
- Według użytkownika `target/` ma ok. 35 GB (release ok. 500 MB), więc problem dotyczy głównie dev/test. Podział na `debug/deps`, `incremental`, `build` nadal wymaga pomiaru.

## Możliwości

**A. Pomiar (bez zmian w kodzie)**
- A1. `du -sh target/*` oraz `target/debug/{deps,incremental,build}` – ustalenie, co dominuje.
- A2. `cargo install cargo-sweep` / `cargo-cache`; `cargo bloat`/`cargo-diet` zbędne na tym etapie.

**B. Mniej informacji debug (największy zysk w dev/test, zwykle 30–60%)**
- B1. Dla zależności wystarczą numery linii: `[profile.dev.package."*"] debug = "line-tables-only"`. Własny kod wymaga pełnego debug – **nie** obniżamy `debug` dla crate'ów workspace'u.
- B2. `[profile.dev] split-debuginfo = "unpacked"` (Linux: tylko przy wsparciu toolchaina) – mniej danych w linkowanych plikach.
- B3. Alternatywnie `[profile.dev.package."*"] debug = false` – bez debug dla zależności (własny kod zostaje w pełni debugowalny).
- B4. `[profile.release] strip = "symbols"` – mniejsze binarki w `dist/` i `target/release`.
- Koszt: gorsze debugowanie zależności w gdb/lldb (B1, B3); B4 usuwa symbole z backtrace'ów panik w binarkach release (sprawdzić z założeniem o `dist/` poniżej). Zysk bez wpływu na zachowanie programu.

**C. Mniej osobnych plików wykonywalnych testów**
- C1. Połączenie ok. 65 testów integracyjnych w jeden crate testowy (`tests/integration/main.rs` + moduły) – jeden link zamiast ok. 65; zwykle największy pojedynczy zysk, także na czasie budowania.
- Koszt: refaktor struktury testów, możliwe konflikty nazw/globali, jednorazowa praca.

**D. Mniej wariantów buildów**
- D1. Spójne flagi i profile między `run.sh`, CI i lokalnym developmentem, żeby nie powstawały równoległe zestawy artefaktów (np. `--release` tylko tam, gdzie potrzebne).
- D2. `cargo build -p <crate>` zamiast `--workspace` w typowych przepływach.
- D3. Ograniczenie zbędnych cech (`tokio` `features = ["full"]` → konkretne) – mniej kodu do kompilacji i przechowania.
- D4. Przegląd drzewa zależności (`cargo tree -d`) pod kątem duplikatów wersji.

**E. Higiena i sprzątanie**
- E1. Okresowe `cargo clean` lub `cargo sweep --time 14` (usuwa artefakty starsze niż N dni) – lokalnie i jako krok cron/CI.
- E2. `cargo clean -p <crate>` przy pracy nad jednym crate'em.
- E3. Wyłączenie incremental w CI: `CARGO_INCREMENTAL=0` (mniejszy katalog `incremental/`, zwykle też szybszy czysty build).
- E4. W CI cache tylko `~/.cargo/registry` i wybrane części `target/` (np. `Swatinem/rust-cache`, który sam czyści stare wpisy) zamiast całego `target/`. Uwaga: `ci.yml` i `release.yml` używają `actions-rust-lang/setup-rust-toolchain`, które domyślnie włącza ten cache – przed zmianą sprawdzić, czy E4 już nie działa.

**F. Współdzielenie katalogu**
- F1. Wspólny `CARGO_TARGET_DIR` (lub `build.target-dir`) dla wielu kopii roboczych/worktree – jeden zestaw artefaktów zamiast N. Ważne przy równoległych workspace'ach agentów.
- F2. Cache kompilacji `sccache` – zmniejsza ponowne budowanie, ale nie sam rozmiar `target/`; wymaga osobnego cache na dysku.
- **Odrzucone przez użytkownika:** wspólny `CARGO_TARGET_DIR` – w środowiskach agentów (np. GitHub) nie ma nad tym kontroli, a ryzyko konfliktów między agentami jest zbyt duże.
- Koszt F1 (informacyjnie): blokady Cargo serializują równoległe buildy; ryzyko mieszania wersji toolchaina.

**G. Zmiana linkera / kompresja (opcjonalnie)**
- G1. Szybszy linker (`mold`/`lld`) skraca czas, nieznacznie wpływa na rozmiar.
- G2. Kompresja systemu plików (btrfs/zstd) lub osobny wolumin na `target/`.

## Rekomendacja (kolejność)

1. A1 – zmierzyć, co zajmuje miejsce.
2. B1 (lub B3) tylko dla zależności (+ B4 dla release) – zmiana tylko w `Cargo.toml`, niskie ryzyko; własny kod bez zmian debug.
3. E3 + E4 w CI.
4. C1 (zgoda użytkownika) – przy ok. 35 GB prawdopodobnie największy zysk; można wykonać zaraz po pomiarze.

## Requirements (do zatwierdzenia)

- R1. Zmierzony podział rozmiaru `target/` jest dołączony do decyzji.
- R2. Zmiany profili nie zmieniają zachowania produkcyjnych binarek poza rozmiarem (bez zmiany `opt-level`/`panic` bez osobnej zgody).
- R3. `cargo test --workspace` i `cargo build --workspace --bins --release` nadal przechodzą.
- R4. Zmiany w CI nie wydłużają istotnie czasu czystego builda.
- R5. Decyzja właściciela: testy zależne od infrastruktury, uruchamiające dodatkowe procesy lub zależne od innych komponentów są usuwane (nie zabezpieczane), a ich lista trafia do opisu PR; R3 dotyczy testów, które zostają.

## Założenia do potwierdzenia

- Celem jest rozmiar na dysku (lokalnie i/lub CI), nie rozmiar artefaktów wydawniczych w `dist/`.
- Utrata pełnego debug info dla zależności jest akceptowalna (zostają numery linii); dla własnego kodu pełne debug info zostaje.
- Rekomendacje opierają się na typowych proporcjach dla Rust workspace'ów, dopóki nie ma podziału rozmiaru z A1.
- Refaktor testów (C1) jest zatwierdzony przez użytkownika; kolejność po pomiarze.

## Rozstrzygnięte pytania

- Rozmiar: `target/` ok. 35 GB, release ok. 500 MB; „wszystko jest bolesne” (lokalnie, CI i cache).
- Debugger: w zależnościach wystarczą numery linii, w własnym kodzie potrzebne pełne debug info.
- Wspólny `CARGO_TARGET_DIR`: nie (brak kontroli w środowiskach agentów, ryzyko konfliktów).
- Połączenie testów integracyjnych w jeden crate (C1): tak.
