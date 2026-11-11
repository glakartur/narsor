# Dobór modelu i effortu per krok workflow — pomysł na rozwój

Status: pomysł. Brak planu i brak decyzji o implementacji.
Dotyczy: `workflows/compound-engineering.yaml` i dyrygenta.

## Cel

Każdy krok workflow używa modelu i effortu dopasowanych do jego trudności.
Koszt spada przy zadaniach prostych. Jakość rośnie przy zadaniach trudnych.

## Założenie wyjściowe

Dyrygent dobiera ustawienia raz, po planie. Nie pyta o nie przed każdym krokiem.
Plan zawiera rozmiar zadań, zależności i ryzyka. Brainstorm tego nie zawiera.

## Propozycja

1. **Wartości domyślne w definicji workflow.** Każdy krok ma domyślny model i effort. Wartości są dane, nie logika. Testy workflow sprawdzają tylko, że pole istnieje i ma poprawny kształt. Testy nie sprawdzają wartości ani treści promptów.
2. **Profil po planie.** Dyrygent czyta plan i zapisuje profil dla każdego kroku, np. `ce-work: unit 3 → opus/high`. Decyzja zapada raz. Profil jest zapisany w danych zadania i widoczny w TUI.
3. **Zejście w dół swobodne.** Dyrygent może obniżyć model lub effort o dowolną liczbę stopni. Przykład: krok mechaniczny (commit, push) może dostać najtańszy model, bez względu na wartość domyślną.
4. **Wejście w górę ograniczone.** Dyrygent może podnieść ustawienie tylko do górnej granicy zdefiniowanej w workflow. Wyjście poza granicę (np. `max`, najdroższy model) wymaga zgody człowieka.
5. **Eskalacja przy błędzie — późna.** Eskalacja nie następuje przy drugiej próbie. Pierwsza i druga próba używają tego samego profilu, bo błąd bywa przypadkowy. Eskalacja następuje przy trzeciej lub czwartej nieudanej próbie tego samego kroku. Dokładny próg jest parametrem konfiguracji. Eskalacja podnosi effort, potem model, w granicy z punktu 4.

## Co dyrygent ocenia wiarygodnie

- liczba plików i modułów w zakresie zadania,
- obecność migracji lub zmian kontraktów,
- brak testów w obszarze zmiany,
- zależności między jednostkami planu.

Dyrygent nie ocenia ukrytej trudności. Dlatego potrzebna jest eskalacja z punktu 5.

## Punkt wyjścia dla wartości domyślnych

Wartości są hipotezą. Nie zostały zmierzone na zadaniach narsora.

| Krok | Model | Effort |
|---|---|---|
| brainstorm | Opus 5.5 | high |
| plan | Opus 5.5 | high |
| doc-review | Sonnet 5.5 | medium |
| work (zadanie jasne) | Sonnet 5.5 | medium |
| work (zadanie trudne) | Opus 5.5 | high |
| simplify-code | Sonnet 5.5 | medium |
| code-review | Opus 5.5 | high |
| debug | Opus 5.5 | high |
| test-browser | Sonnet 5.5 | low–medium |
| resolve-pr-feedback | Sonnet 5.5 | medium |
| commit / push / PR | Haiku 5.5 | low |
| compound | Opus 5.5 | medium |

## Otwarte pytania

- Gdzie runtime przekazuje dziś model i effort agentowi? Czy każdy backend (Claude Code, Codex, OpenCode, Copilot SDK) obsługuje oba parametry?
- Jaka jest dokładna wartość progu eskalacji (trzecia czy czwarta próba)?
- Jak mierzyć wynik? Propozycja: uruchomić 2–3 realne zadania na różnych profilach i porównać liczbę rund przeglądu oraz koszt.
- Czy profil ma być edytowalny przez człowieka w bramce po planie?

## Ryzyka

- Zbyt niskie ustawienia po planie zwiększają liczbę nieudanych prób. Późna eskalacja wydłuża ten koszt. Pomiar z poprzedniej sekcji ma to ocenić.
- Dyrygent może źle ocenić trudność. Granica górna i eskalacja ograniczają skutki.

## Realizacja

Plan: `docs/plans/2026-11-11-1000-feat-dynamiczny-model-i-effort-kroku-plan.md`.
