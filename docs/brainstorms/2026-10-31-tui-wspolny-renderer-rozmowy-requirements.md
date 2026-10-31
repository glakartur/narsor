---
date: 2026-10-31
topic: tui-wspolny-renderer-rozmowy
---

# Wspólny mechanizm renderujący rozmowę w zakładkach Przebieg i Rozmowa

## Summary

Zakładki *Przebieg* (rozmowa wybranego kroku) i *Rozmowa* (cały przebieg) pokazują tę samą rozmowę przez jeden mechanizm renderujący. Mechanizm dostaje na wejściu zakres (cały przebieg albo jeden krok) oraz położenie i wymiary okna. Wpisy „WYBÓR WORKFLOW” (kto wybrał wyjście z kroku) są widoczne w obu zakładkach.

## Problem Frame

Dziś są dwie ścieżki budowy i rysowania transkryptu:

- *Rozmowa* (`Conversation::rebuild_with_events`, `render` w `tabs/conversation.rs`) składa rozmowy wszystkich kroków. Dodaje nagłówki `── <krok> ──` oraz wpisy wyboru workflow (agent, dyrygent, operator) z zdarzeń. Wpis wyboru stoi po rozmowie wejścia w krok, którego dotyczy (patrz `2026-10-18-rozmowa-chronologia-wyborow-workflow-requirements.md`).
- *Przebieg* (`render_step` / `render_attempt` w `tabs/progress.rs`) rysuje transkrypt jednej próby (`Attempt`) bez zdarzeń. Nie ma w nim wyborów workflow, więc operator nie widzi, kto i co wybrał na wyjściu z kroku.

Obie ścieżki wołają `render_transcript`, ale budowa treści jest różna, a szerokość i wysokość są wyprowadzane lokalnie w każdej z nich. Skutek: różna treść w dwóch widokach tej samej rozmowy i trudność z jednolitym skracaniem linii.

## Requirements

**Wybory workflow w obu widokach**
- R1. W zakładce *Przebieg* rozmowa wybranego kroku zawiera wpisy wyboru workflow, które następują po wejściu w ten krok (agent, dyrygent, operator). Treść, nagłówek, kolor i identyfikator wpisu są takie same jak w *Rozmowie*.
- R2. Wpis wyboru jest w tym samym miejscu względem rozmowy kroku w obu widokach (po rozmowie wejścia w krok, zasady kolejności z dokumentu o chronologii wyborów).
- R3. Zestaw zdarzeń dających wpis wyboru jest bez zmian.

**Zakres renderowania**
- R4. Mechanizm renderujący przyjmuje na wejściu zakres: *cały przebieg* albo *jeden krok*.
- R5. Dla zakresu *cały przebieg* mechanizm dokłada rozdzielniki kroków (nagłówki `── <krok> ──`, jak dziś w *Rozmowie*). Dla zakresu *jeden krok* rozdzielników kroków nie ma.
- R6. Zakres *jeden krok* oznacza wejście w krok (wszystkie jego próby), nie pojedynczą próbę. Wybór po tym wejściu należy do tego zakresu.
- R7. *Rozmowa* używa zakresu *cały przebieg*, *Przebieg* zakresu *jeden krok* (wybrany krok na osi).

**Okno: położenie i wymiary**
- R8. Mechanizm renderujący dostaje na wejściu położenie okna na ekranie (x, y) i jego wymiary (szerokość, wysokość). Nie wylicza ich sam z kontekstu zakładki.
- R9. Mechanizm używa wymiarów do dopasowania treści: zbyt długie linie są skracane (z wielokropkiem) tam, gdzie dziś zawijanie nie ma sensu (nagłówki, rozdzielniki, wpisy wyboru, plakietki), a treść rozmowy jest zawijana do szerokości okna. Wysokość służy do okna przewijania.
- R10. Zmiana wymiarów okna (zmiana rozmiaru terminala, szerokość osi w *Przebiegu*) przelicza układ bez utraty kursora, rozwiniętych wpisów i trybu „podąża”.

**Bez zmian**
- R11. Zachowanie kursora, rozwiniętych wpisów, trybu „podąża”, trybów wyświetlania (rozmowa, chronologia, RAW) i skrótów klawiszowych jest bez zmian.
- R12. Zakładki *Zdarzenia*, *Dyrygent* i *Info* nie zmieniają zachowania. *Dyrygent* też woła `render_transcript` (`tabs/conductor.rs`), więc jego wynik wizualny zostaje ten sam.
- R13. Zmiana działa tak samo w szczegółach aktywnego zadania i w Historii.

## Acceptance Examples

- AE1. **Covers R1, R2, R7.** Krok `code_review` skończył się wyborem agenta `revise`. W *Przebiegu* po wybraniu `code_review` rozmowa kończy się wpisem „WYBÓR WORKFLOW” z `revise`. W *Rozmowie* ten sam wpis stoi po rozmowie `code_review` i przed nagłówkiem następnego kroku.
- AE2. **Covers R5, R6.** Pętla `code_review → fix → code_review`. W *Przebiegu* wybór drugiego wejścia pokazuje tylko wybory drugiego wejścia. W *Rozmowie* są nagłówki kroków, w *Przebiegu* ich nie ma.
- AE3. **Covers R9.** Wąskie okno (np. 40 kolumn): nagłówek kroku i wpis wyboru z długą etykietą nie łamią się na kilka wierszy, tylko kończą się wielokropkiem. Treść rozmowy zawija się.
- AE4. **Covers R10.** Operator zmienia rozmiar terminala w trakcie czytania rozwiniętego wpisu. Kursor zostaje na tym samym wpisie.

## Success Criteria

- Operator widzi decyzje o wyjściu z kroku w obu zakładkach, w identycznej postaci.
- Jedna ścieżka budowy i rysowania rozmowy obsługuje oba zakresy. Nie ma kopii logiki wyborów w *Przebiegu*.
- Testy: zakres *krok* vs *przebieg* (obecność rozdzielników), wybory w obu zakresach (AE1, AE2), skracanie w wąskim oknie (AE3).

## Scope Boundaries

- Nie zmieniamy treści ani formatu wpisu „WYBÓR WORKFLOW” ani formatu nagłówka kroku.
- Nie dodajemy nowych typów zdarzeń ani zmian w API/protokole.
- Nie zmieniamy osi kroków w *Przebiegu* (lista, status, czasy).
- Nie zmieniamy zakładek *Zdarzenia*, *Dyrygent*, *Info*.

## Key Decisions

- Jeden mechanizm z parametrem zakresu zamiast dwóch ścieżek. Różnica między widokami to tylko zakres i okno.
- Zakres *krok* to wejście w krok, spójnie z regułą „wejście w krok” z dokumentu o chronologii wyborów.
- Okno (położenie, wymiary) jest wejściem, nie wynikiem. Dzięki temu mechanizm jest testowalny bez ramki terminala.

## Założenia do potwierdzenia

- Zdarzenia (`view.events`) są dostępne przy rysowaniu *Przebiegu* tak jak w *Rozmowie*. Jeśli nie, trzeba je dostarczyć do stanu *Przebiegu* (do weryfikacji w planowaniu).
- „Rozdzielniki kroków” to istniejące nagłówki `── <krok> ──`. Nie wprowadzamy nowego wyglądu.
- „Położenie okna” to prostokąt (x, y, szerokość, wysokość) w układzie ramki ratatui; mechanizm używa go do rysowania i do przeliczenia wierszy wyboru, a nie do własnego pozycjonowania.
- Skracanie dotyczy linii, które dziś są jednowierszowe z natury (nagłówki, plakietki, wybory). Treść wiadomości dalej się zawija, żeby nic nie ginęło.
- W *Przebiegu* zakres obejmuje wszystkie próby wejścia w krok, spójnie z regułą wejścia, a nie tylko wybraną próbę osi.

## Pytania otwarte dla użytkownika

- Czy w *Przebiegu* wpis wyboru ma być także, gdy krok nadal trwa i wyboru jeszcze nie ma? Założenie: nie, wpis pojawia się po zdarzeniu wyboru.
- Czy skracanie ma dotyczyć także długich linii treści rozmowy (zamiast zawijania)? Założenie: nie, treść się zawija.

## Outstanding Questions

### Deferred to Planning

- [Affects R4, R8][Technical] Kształt API mechanizmu: struktura wejścia (zakres + `Rect`) i wspólny typ wyniku dla `Conversation` i `Attempt`.
- [Affects R1, R6][Technical] Jak wskazać wejście w krok dla wybranej pozycji osi (`state_id` + `visit`).
- [Affects R10][Technical] Czy stan przewijania per zakres jest zachowywany osobno dla każdego kroku w *Przebiegu*.
