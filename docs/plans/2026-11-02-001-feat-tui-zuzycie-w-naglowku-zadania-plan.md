---
title: TUI Zużycie tokenów i koszt w nagłówku zadania - Plan
type: feat
date: 2026-11-02
execution: code
---

# TUI: zużycie tokenów i koszt w nagłówku szczegółów zadania

## Co i dlaczego

Nagłówek szczegółów zadania (`src/tui/task_app/detail.rs`, `header()`) dostaje
dodatkowy wiersz z sumarycznym kosztem i tokenami in/out całego zadania oraz
osobno częścią subagentów. Dane już są w timeline: `Progress::timeline()` →
`Position.usage` (`PositionUsage`: `total`, `subagents`, `subagent_count`,
`cost_usd`). Timeline jest czytany zawsze, więc nie trzeba nowych odczytów ani
zmian w protokole.

Przykład: `$1.20 · in 120k out 8k · w tym subagenci in 40k out 3k (2)`

## Decyzje

- Suma obejmuje wszystkie kroki zadania; subagenci są wliczeni w sumę i pokazani
  osobno (decyzja użytkownika).
- `in` / `out` to dokładnie pola `input` / `output` z `TokenCounts`, tak jak
  zwrócił je backend. `cache_read` / `cache_write` nie są doliczane ani
  interpretowane, a UI i dokumentacja nie deklarują, co `in` obejmuje.
- Brak raportu: część nieznana pokazuje `—`; gdy żaden krok nie ma zużycia,
  wiersza nie ma. Człon subagentów znika, gdy `subagents` to `None` lub zero.

## Jednostki

### 1. Agregacja i tekst zużycia

- Nowy moduł formatowania obok `src/client/usage_text.rs` (wspólny dla TUI/CLI,
  zgodnie z jego wzorcem), rejestrowany w `src/client_lib.rs`.
- Sumowanie po `positions` przez `TokenCounts::plus` (`total`, `subagents`),
  `subagent_count` i `cost_usd`; wynik jako `Option`, `None` gdy nic nie podano.
- Skala `k` / `M`, koszt w USD.
- Testy (w module): suma wielu kroków; krok bez `usage`; brak danych w ogóle;
  progi `k`/`M`; zaokrąglenie kosztu; `cache_*` nie wpływa na `in`.

### 2. Render w nagłówku

- `src/tui/task_app/detail.rs`: `header()` przyjmuje timeline z `Progress` i
  zwraca czwartą linię (po linkach, więc indeks linków w `link_spots`,
  `header.y + 3`, się nie zmienia); `HEADER_LINES` rośnie o 1 tylko gdy wiersz
  jest pokazany (inaczej układ bez danych zostaje jak dziś).
- Skracanie przez `fit_cells`: najpierw gaśnie człon subagentów, potem reszta.
- Styl `Role::Done`/`Calm`; w stanie `stale` szary, jak reszta nagłówka.
- Testy w `detail.rs`: nagłówek z danymi; bez danych (układ bez zmian); wąski
  terminal; `stale`; notice + nowa linia (wysokość `areas`).

### 3. Dokumentacja

- Jeśli `docs/user` opisuje nagłówek szczegółów: jedno zdanie, że wiersz pokazuje
  zużycie raportowane przez harness. Bez definicji `in`.

## Weryfikacja

`cargo test -p narsor tui::task_app::detail` oraz testy nowego modułu formatowania.
