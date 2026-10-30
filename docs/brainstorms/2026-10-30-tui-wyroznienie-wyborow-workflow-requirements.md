---
date: 2026-10-30
topic: tui-wyroznienie-wyborow-workflow
---

# Wyróżnienie wyborów workflow w zakładce Rozmowa

## Summary

Każdy wpis „WYBÓR WORKFLOW” w zakładce *Rozmowa* ma tło w innym kolorze niż reszta transkryptu. Bez koloru wpis jest odwrócony (inwersja). Operator widzi od razu, w którym miejscu workflow podjął decyzję i jaka to była decyzja.

---

## Problem Frame

Krok `brainstorm` przeszedł do `doc_review` bez zapisanego dokumentu. Agent przysłał odpowiedź z `choice: requirements_drafted`. Zapis raw pokazuje obiekt z kluczem `choice` na końcu tekstu agenta. Zapis nie pokazuje `structured_output`. Silnik, według kodu (`read_response` w `crates/narsor-engine/src/engine/actions/agent.rs`), czyta wtedy ostatni obiekt z `choice` z tekstu i przyjmuje go. Zdarzeń silnika dla tego wykonania nie sprawdzono. Odpowiedź agenta nie była więc brakiem `choice`. Workflow poszedł dalej, bo `choice` był poprawny formalnie.

Operator nie zauważył tego w TUI. Wpis „WYBÓR WORKFLOW · HH:MM:SS” wygląda jak inne wpisy transkryptu: ma tylko kolor nagłówka. W długiej *Rozmowie* wpis ginie między rozmowami kroków. Operator widzi „przeszło do doc_review”, ale nie widzi w jednym spojrzeniu, kto wybrał to przejście, jaką etykietą i z jakim uzasadnieniem.

Wpis wyboru powstaje z zdarzeń: `DecisionMade` (agent), `ConductorDecision` typu `ChooseOutput` (dyrygent), `HumanInputReceived` z etykietą wyjścia i `HumanTransitionSelected` (użytkownik) — `src/tui/task_app/tabs/conversation.rs`. Treść wpisu to „<aktor> wybrał opcję: <etykieta>” i opcjonalnie „uzasadnienie: …”.

---

## Requirements

**Wyróżnienie**

- R1. Każdy wpis „WYBÓR WORKFLOW” w *Rozmowie* ma tło w kolorze innym niż tło transkryptu. Tło obejmuje nagłówek i wszystkie linie treści wpisu.
- R2. Tło sięga do prawej krawędzi panelu w każdej linii wpisu, także gdy tekst jest krótszy. Wpis jest czytelnym blokiem.
- R3. Gdy kolor jest wyłączony (`NO_COLOR`), wpis jest wyróżniony inwersją (odwrócone kolory) zamiast tła. Zasada jest ta sama co dla innych wyróżnień w TUI w tym trybie.
- R4. Kolor tła wpisu różni się od innych teł w TUI: od zaznaczenia (`selection_fill`), od nagłówka zadania, od panelu odpowiedzi i od pola edycji. Operator nie myli wyboru workflow z zaznaczeniem.
- R5. Wyróżnienie jest takie samo dla wyboru agenta, dyrygenta i użytkownika. Kolor tekstu nagłówka nadal wskazuje aktora (jak dziś).
- R6. Zaznaczony wpis wyboru pokazuje oba sygnały: pasek zaznaczenia i tło wyboru. Tło nie ukrywa paska.

**Zakres wyróżnienia**

- R7. Wyróżnienie dotyczy tylko wpisów zbudowanych ze zdarzeń wyboru z sekcji *Problem Frame*. Tekst agenta, w którym jest słowo „choice” lub obiekt JSON, nie dostaje wyróżnienia.
- R8. Wyróżnienie działa tak samo w szczegółach aktywnego zadania i w Historii.

**Bez zmian**

- R9. Treść, nagłówek, identyfikator, kolejność i zestaw zdarzeń wpisu wyboru są bez zmian.
- R10. Kursor, rozwinięcie wpisów, tryb „podąża” i zaznaczanie tekstu działają jak dziś.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Dane: agent kroku `brainstorm` wybrał `requirements_drafted` z uzasadnieniem w dwóch liniach. Wynik: nagłówek i trzy linie wpisu mają to samo tło do prawej krawędzi panelu. Rozmowy sąsiednich kroków nie mają tego tła.
- AE2. **Covers R3.** Dane: `NO_COLOR=1`. Wynik: wpis wyboru jest odwrócony. Rozmowy kroków nie są odwrócone.
- AE3. **Covers R5.** Dane: wybór agenta, wybór dyrygenta i wybór użytkownika w jednej *Rozmowie*. Wynik: wszystkie trzy mają to samo tło. Nagłówki mają kolory zielony, magenta i cyjan jak dziś.
- AE4. **Covers R6.** Dane: operator zaznacza wpis wyboru. Wynik: widać pasek zaznaczenia i tło wyboru.
- AE5. **Covers R7.** Dane: agent w rozmowie napisał tekst `{"choice": "x"}` w treści wiadomości. Wynik: wiadomość nie ma tła wyboru.
- AE6. **Covers R8.** Dane: to samo zadanie otwarte w szczegółach i w Historii. Wynik: wpisy wyboru wyglądają tak samo w obu miejscach.

---

## Success Criteria

- Operator przewija *Rozmowę* i znajduje każdą decyzję workflow bez czytania treści wiadomości.
- W zdarzeniu z `brainstorm` operator widzi od razu blok „agent wybrał opcję: requirements_drafted” z uzasadnieniem „Dokumentu nie zapisano”. Operator zatrzymuje workflow przed `doc_review`.
- Wyróżnienie jest czytelne z kolorem i bez koloru.

---

## Scope Boundaries

- Poza zakresem: zmiana treści, kolejności lub formatu wpisu wyboru.
- Poza zakresem: wyróżnienia w zakładkach *Przebieg*, *Zdarzenia* i *Dyrygent* oraz na liście zadań.
- Poza zakresem: zmiany w silniku. Dwie wcześniej rozważane zmiany zostają jako osobne tematy, bo nie dotyczą wyświetlania:
  - kontrola, czy plik wymaganego artefaktu istnieje (dziś silnik sprawdza tylko niepustą ścieżkę: `unmet_requirements` w `crates/narsor-engine/src/engine/runner.rs`);
  - jawna odpowiedź agenta „krok nie został wykonany”, która kończy krok błędem.
- Poza zakresem: wykrywanie, czy uzasadnienie agenta zgadza się z jego wyborem.

---

## Key Decisions

- Wyróżnienie przez tło (kolor) i inwersję (bez koloru). Takie rozwiązanie wskazał użytkownik. Tło obejmuje cały wpis, bo sam kolor nagłówka dziś nie wystarcza.
- Jeden wygląd dla wszystkich aktorów. Aktor jest widoczny w kolorze nagłówka i w treści. Dwa sygnały naraz (tło dla aktora i tło dla wyboru) zwiększyłyby liczbę teł w TUI.

---

## Założenia do potwierdzenia

- Sformułowanie „wybrane tak akcje workflow” oznacza wpisy „WYBÓR WORKFLOW” w zakładce *Rozmowa*. Nie sprawdzono innej interpretacji (np. wybrane przejście w zakładce *Przebieg*).
- Agent w opisanym zdarzeniu przysłał `choice`. Wniosek pochodzi z zapisu raw i z kodu silnika. Zdarzeń `ActionCompleted` i `DecisionMade` dla tego wykonania nie sprawdzono.
- Wyróżnienie dotyczy wszystkich wyborów (agent, dyrygent, użytkownik), nie tylko wyborów agenta.
- Wpis nieaktualny (`stale`, wyszarzony) zachowuje tło wyboru, ale z wyszarzoną treścią.
- Konkretny kolor tła wybiera planowanie. Warunek: spełnia R4.
- Treść zgłoszenia WRUW-6889 jest niedostępna (konektor Atlassian wymaga autoryzacji). Dokument nie opiera się na tym zgłoszeniu.

---

## Pytania otwarte dla użytkownika

- Czy wyróżnić tylko wybory agenta, czy wszystkie? Dokument przyjmuje wszystkie.
- Czy wyróżnienie ma dotyczyć też innego miejsca niż *Rozmowa* (np. wybrane przejście w *Przebiegu*)? Dokument przyjmuje tylko *Rozmowę*.
- Czy zmiany w silniku z sekcji *Scope Boundaries* (kontrola pliku artefaktu, odpowiedź „krok nie został wykonany”) mają dostać osobne zgłoszenia?

---

## Outstanding Questions

**Do planowania**

- Jak oznaczyć wpis wyboru w modelu transkryptu, żeby renderer wyróżniał go bez rozpoznawania po nagłówku (`TranscriptEntry` w `src/tui/transcript.rs`).
- Jak narysować tło do prawej krawędzi przy zawijaniu długich linii i przy wpisach zwiniętych lub rozwiniętych.
- Który kolor tła spełnia R4 na terminalach z 256 kolorami i na terminalach z 16 kolorami.
