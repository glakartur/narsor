# Podniesienie effortu w sesji przy braku postępu — pomysł na rozwój

Status: pomysł. Brak planu i brak decyzji o implementacji.
Dotyczy: `crates/narsor-engine/src/engine/step_profile.rs`, `crates/narsor-engine/src/engine/runner.rs`, dyrygent (`crates/narsor-engine/src/conductor/`).
Zastępuje: odrzucony plan `docs/plans/2026-11-11-002-feat-eskalacja-effort-plus-dwa-potem-model-plan.md`.

## Cel

Gdy agent nie robi postępu, krok dostaje wyższy effort. Model zostaje ten sam.
Koszt rośnie mało, bo rozmowa nie przechodzi do nowego modelu.

## Dlaczego nie obecna eskalacja

- Eskalacja z PR #92 liczy nieudane próby w pętli `retry_policy`. Zlicza tylko porażki techniczne: błąd harnessu, błąd uruchomienia, timeout.
- Zły format odpowiedzi nie zużywa `retry_policy`. Obsługuje go reprompt, potem dyrygent, potem operator (R15, D55).
- Domyślnie `max_attempts` to 1, więc eskalacja zwykle nie działa. Szablon workflow ma 2, co też nie wystarcza przy progu 2 lub 3.
- Zmiana modelu w trakcie pracy zwiększa koszt. Nowy model nie korzysta z cache'u rozmowy, więc całość idzie po pełnej cenie. (Wiedza ogólna o cache'u promptu, nie sprawdzona w narsorze.)

## Przypadki, które ten pomysł ma pokryć

| Przypadek | Co widać w systemie | Kto to widzi |
|---|---|---|
| Agent nie odpowiada lub odpowiada tym samym | Powtarzana odpowiedź w tej samej sesji | Dyrygent przy autopilocie |
| Agent dopytuje usera o to samo | Powtarzane pytanie w rozmowie | Dyrygent przy autopilocie. Bez autopilota odpowiada człowiek. |
| Pętla kod, code-review | Kolejne wejścia w krok kodu | Silnik (licznik wejść). Dyrygent tylko gdy to on decyduje o przejściu. |
| Pętla kod, testy w kroku `shell` | Skrypt zawodzi i workflow wraca do kroku kodu | Silnik. Dyrygent nie jest wywoływany. |
| Jeden długi przebieg, agent "walczy" z testami | Brak porażki, brak sygnału w silniku | Nikt dziś. Potrzebny budżet czasu lub tokenów. |

Wniosek: sam dyrygent nie widzi wszystkich pętli. Część sygnałów ma tylko silnik.

## Propozycja

1. **Effort per wejście w krok, nie per próba.** Przy ponownym wejściu w krok agenta silnik może zastosować wyższy effort. Licznik wejść już istnieje (`max_entries`, `state_entries`).
2. **Wniosek dyrygenta.** Przy wywołaniu dyrygenta (odpowiedź na pytanie agenta, decyzja, rescue) dyrygent może zapisać prośbę "podnieś effort kroku X". Silnik stosuje ją przy następnym wejściu lub wznowieniu sesji. Dyrygent nie zapisuje już profilu kroków. Robi to krok `profile` (plan `docs/plans/2026-11-11-002-feat-krok-profile-workflow-plan.md`), więc ta prośba potrzebuje własnego miejsca zapisu albo zapisu w `step-profile.json`.
3. **Sufit.** Podniesienie nie przekracza `ceiling` kroku (zasady z PR #92 zostają).
4. **Model stały.** Zmiana modelu jest poza zakresem. Wraca tylko jako świadoma decyzja operatora.
5. **Wyzwalacz w silniku.** Dla pętli, które dyrygent nie widzi (reguła, skrypt), silnik może podnieść effort po N wejściach w ten sam krok. Wartość N jest do ustalenia.
6. **Stop zamiast mocy.** Przy powtarzanym pytaniu o to samo częściej brakuje informacji niż mocy modelu. Dyrygent powinien móc wybrać: podnieś effort albo zatrzymaj i zapytaj operatora.

## Co trzeba sprawdzić przed planem

- Czy ponowne wejście w krok agenta tworzy nową sesję, czy wznawia starą. Od tego zależy, czy "effort w tej samej sesji" w ogóle dotyczy pętli kodu.
- Czy claude-code i codex zmieniają effort wznowionej sesji. Adaptery przekazują effort razem z `--resume` (claude-code) i `resume` (codex), ale skutek nie był sprawdzony na żywo.
- Czy zmiana effortu unieważnia cache rozmowy u dostawcy. Jeśli tak, oszczędność jest mniejsza.
- Czy dyrygent widzi historię wejść w krok (od drugiego wejścia trafia ona do promptu kroku agenta).
- Czy podniesienie effortu pomaga w pętlach. Wymaga pomiaru na prawdziwych zadaniach. Dla pętli "pytanie o to samo" jest to wątpliwe.

## Powiązane

- Jedno ograniczenie, które już istnieje: `max_entries` zatrzymuje pętlę i czeka na decyzję operatora. Nie podnosi effortu.
- Ochrona przed brakiem postępu (domyślny limit wejść, wykrywanie powtórzeń, budżet przebiegu) jest osobnym tematem i może być potrzebna niezależnie od effortu.
- `docs/solutions/architecture-patterns/step-profile-tiers-decide-once-validate-at-load-2026-11-11.md`: zasady profilu (poziomy, sufit, decyzja raz, walidacja przy ładowaniu), które ten pomysł musi zachować lub świadomie zmienić ("decyzja raz" nie pasuje do zmiany effortu między wejściami).
- `docs/follow-ups/2026-10-28-dobor-modelu-i-effortu-per-krok-workflow.md`: pomysł wyjściowy.
- `docs/brainstorms/2026-11-10-dynamiczny-model-i-effort-kroku-requirements.md`: R9, R10 i AE4 opisują dzisiejszą eskalację i zostają bez zmian.
