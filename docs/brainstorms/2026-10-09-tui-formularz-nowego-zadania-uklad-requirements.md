---
date: 2026-10-09
topic: tui-formularz-nowego-zadania-uklad
---

# Formularz nowego zgłoszenia: układ i nowa linia

## Summary

Po wyborze workflow lista z opisem zwija się do wąskiego paneliku na górze, a cały pozostały obszar przejmuje formularz: wąski pasek z repo i krótkimi wejściami oraz jedno duże, wieloliniowe pole promptu. Nowa linia w polach tekstowych działa w każdym terminalu (`Ctrl+J`), a `Shift+Enter` i `Alt+Enter` — tam, gdzie terminal je rozróżnia.

---

## Problem Frame

Formularz nowego zgłoszenia pokazuje listę workflow i po prawej jego opis, co działa dobrze. Po wyborze workflow lista i opis dalej zajmują większość ekranu, a wejścia workflow (repo, prompt) dostają tylko tyle wierszy, ile mają zawartości: prompt zaczyna od jednego wiersza, repo to przełącznik `←`/`→`. Operator wpisuje dłuższą treść w małym polu, choć to ona jest sednem zgłoszenia.

Dodatkowo opisany w podpowiedziach `Alt+Enter` jako „nowa linia" w polu tekstowym zatwierdza formularz. Terminal (lub multiplekser) najpewniej przekazuje tę kombinację jako zwykły Enter, więc TUI nie widzi modyfikatora. Zachowanie zależy od terminala, więc jeden sztywny klawisz oparty na modyfikatorze nie jest niezawodny.

---

## Requirements

**Układ po wyborze workflow**
- R1. Wybór workflow nadal pokazuje listę z opisem po prawej (bez zmian).
- R2. Po wyborze workflow lista i opis zwijają się do krótkiego paneliku na górze (nazwa wybranego workflow, w razie potrzeby krótkie streszczenie).
- R3. Cały pozostały obszar formularza zajmują wejścia workflow: pod panelikiem wąski pasek z repo i krótkimi wejściami (liczby, true/false, krótkie teksty jednoliniowe), a pod nim jedno główne pole tekstowe — prompt — rozciągnięte na resztę wysokości.
- R4. Głównym polem („promptem") jest wieloliniowe wejście tekstowe workflow; gdy jest ich kilka, pierwsze wymagane.
- R5. Wejścia rozmieszczone w pasku i w polu promptu zachowują obecne zasady: wymagane/opcjonalne, walidacja, wartości domyślne, podpowiedzi opisów wejść.
- R6. Z fokusem na panelik `Esc` / `Shift+Tab` wraca do wyboru workflow; wpisany prompt i wybrane repo zostają zachowane.

**Nowa linia w polach tekstowych**
- R7. `Ctrl+J` wstawia nową linię w każdym terminalu.
- R8. Gdy terminal rozróżnia modyfikatory (wspiera rozszerzone raportowanie klawiszy), `Shift+Enter` i `Alt+Enter` też wstawiają nową linię i nie zatwierdzają formularza.
- R9. Pasek podpowiedzi pokazuje klawisze nowej linii, które faktycznie działają w bieżącym terminalu (zawsze `Ctrl+J`, plus `Shift+Enter`/`Alt+Enter` tylko gdy wykryto ich obsługę).
- R10. Zasada dotyczy wszystkich pól tekstowych TUI (nowe zgłoszenie, panel odpowiedzi, zakładka Dyrygent, pozostałe formularze); `Enter` nadal wysyła.
- R11. README, pomoc i podpowiedzi w interfejsie nie opisują już `Alt+Enter` jako jedynego klawisza nowej linii.

---

## Acceptance Examples

- AE1. **Covers R2, R3, R4.** Given workflow z wejściami `repository` i `prompt`, when operator wybiera workflow na liście, then lista i opis zwijają się do paneliku, a poniżej widać pasek z repo i pole promptu zajmujące całą pozostałą wysokość.
- AE2. **Covers R3, R4.** Given workflow z wejściami `ticket_id`, `dry_run`, `repository` i tekstowym `prompt`, when formularz się otwiera, then `ticket_id`, `dry_run` i `repository` są w wąskim pasku, a duże pole dostaje `prompt`.
- AE3. **Covers R6.** Given operator wpisał prompt i wybrał repo, when wraca do wyboru workflow i wybiera ten sam workflow, then prompt i repo są zachowane.
- AE4. **Covers R7, R10.** Given terminal, który przekazuje `Alt+Enter` jako zwykły Enter, when operator naciska `Ctrl+J` w prompcie, then w polu pojawia się nowa linia, a formularz nie jest wysłany.
- AE5. **Covers R8, R9.** Given terminal wspierający rozszerzone raportowanie klawiszy, when operator naciska `Shift+Enter`, then pojawia się nowa linia, a pasek podpowiedzi wymienia `Ctrl+J` i `Shift+Enter`.

---

## Success Criteria

- Operator wpisuje wielowierszowy prompt w dużym polu bez przewijania, a po wyborze workflow nie widzi już listy zajmującej ekran.
- Nowa linia w polu tekstowym działa w terminalu operatora bez zgadywania, a podpowiedź nie obiecuje klawisza, który nie działa.
- Planowanie nie musi zgadywać układu ani wyboru klawiszy: zakres, kolejność pól i zasada klawiszy są ustalone tutaj.

---

## Scope Boundaries

- Pełnoekranowy dwukrokowy wybór (osobno repo, osobno prompt) — odrzucony na rzecz jednego ekranu z paskiem repo.
- Repo jako osobna lista z filtrowaniem — nie w tym zakresie (pasek, jak dziś przełącznik).
- Zmiana semantyki `Enter`: nadal wysyła; odwrócenie modelu (Enter = nowa linia) odrzucone.
- Zmiany w przepływie wyboru workflow i opisu po prawej.

---

## Key Decisions

- Repo i krótkie wejścia w wąskim pasku, prompt na całą resztę: maksimum miejsca na tekst, kosztem mniej przeglądowego wyboru repo przy wielu repozytoriach.
- Jedno główne pole tekstowe (prompt) zamiast wieloliniowości wszystkich pól: układ jest przewidywalny i bez dzielenia miejsca między pola.
- Klawisz nowej linii jednolity w całym TUI: `Ctrl+J` zawsze + `Shift`/`Alt+Enter` gdy terminal je rozróżnia, z podpowiedzią dopasowaną do terminalu — zamiast jednego klawisza zależnego od terminala.

---

## Dependencies / Assumptions

- Przyczyna problemu z `Alt+Enter` to najpewniej brak rozróżnienia modyfikatora przez terminal/multiplekser (niezweryfikowane na konkretnym terminalu operatora); `Ctrl+J` rozwiązuje to niezależnie od przyczyny.
- Wykrywanie obsługi `Shift`/`Alt+Enter` zależy od tego, czy terminal wspiera rozszerzone raportowanie klawiszy; brak wsparcia oznacza tylko `Ctrl+J`.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R2][Technical] Dokładna zawartość paneliku zwiniętego workflow oraz wysokość pozostawiona na prompt w niskich terminalach (poniżej minimalnej wysokości).
- [Affects R4][Needs research] Jak wskazać „prompt", gdy workflow ma kilka wejść tekstowych i żadnego wymaganego (czy workflow może go oznaczyć).
- [Affects R8, R9][Technical] Sposób wykrywania wsparcia rozszerzonych klawiszy w bieżącym terminalu i zachowanie w multiplekserach.
