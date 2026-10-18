---
date: 2026-10-17
topic: tui-wklejanie-wieloliniowe
---

# Wklejanie wieloliniowej treści do pól tekstowych TUI

## Summary

Wklejenie treści z wieloma liniami do pola tekstowego TUI wstawia całą treść do pola, razem ze znakami nowej linii, i nie zatwierdza formularza. Operator może poprawić treść po wklejeniu, a potem wysłać formularz klawiszem `Enter`, jak dotąd.

---

## Problem Frame

Operator często wkleja do formularza nowego zadania treść z innego miejsca (opis zgłoszenia, fragment logu, notatkę). Ta treść ma zwykle kilka linii. Dziś TUI odbiera wklejoną treść jako ciąg pojedynczych naciśnięć klawiszy. Każdy znak nowej linii w treści dochodzi do TUI jako `Enter`, a `Enter` wysyła formularz. Skutek:

- formularz wysyła się po pierwszej linii wklejonej treści, przed czasem;
- operator nie ma możliwości sprawdzenia i poprawienia treści przed wysłaniem;
- reszta wklejonej treści trafia jako kolejne klawisze do następnego widoku i może wywołać w nim nieoczekiwane akcje.

Stan w kodzie (zweryfikowany): `src/tui/mod.rs` włącza tryb raw i rozszerzone raportowanie klawiszy, ale nie włącza trybu wklejania terminala (bracketed paste). Pętla zdarzeń obsługuje tylko zdarzenia klawiszy, a zdarzenia wklejenia ignoruje.

Wcześniejszy dokument `docs/brainstorms/2026-10-09-tui-formularz-nowego-zadania-uklad-requirements.md` ustalił, że `Enter` wysyła, a nowa linia to `Ctrl+J` (oraz `Shift+Enter` / `Alt+Enter`, gdy terminal je rozróżnia). Ten dokument nie zmienia tej zasady. Dotyczy tylko wklejania.

---

## Requirements

**Wklejanie do pól tekstowych**
- R1. Wklejenie treści do pola tekstowego wstawia całą treść w pozycji kursora jako jedną operację. Wklejenie nie wysyła formularza i nie wywołuje innych akcji.
- R2. W polu wieloliniowym znaki nowej linii z wklejonej treści stają się nowymi liniami w polu. Końce linii w stylu Windows (`\r\n`) i pojedyncze `\r` stają się jedną nową linią.
- R3. W polu jednoliniowym znaki nowej linii z wklejonej treści stają się spacjami. Pole zostaje jednoliniowe.
- R4. Po wklejeniu kursor jest na końcu wklejonej treści, a fokus zostaje w tym samym polu. Operator może edytować treść i wysłać formularz klawiszem `Enter`.
- R5. Pozostałe znaki z wklejonej treści (np. tabulatory, polskie znaki, emoji) trafiają do pola bez zmian, tak samo jak przy wpisywaniu z klawiatury.

**Zakres pól**
- R6. Zasada dotyczy wszystkich pól tekstowych TUI: formularza nowego zadania, panelu odpowiedzi, zakładki Dyrygent i pozostałych formularzy z polami tekstowymi.
- R7. Wklejenie, gdy fokus nie jest w polu tekstowym (np. na liście zadań, na liście workflow, na przełączniku repo), nie wywołuje żadnej akcji. Treść jest ignorowana.

**Terminal**
- R8. TUI włącza tryb wklejania terminala przy starcie i wyłącza go przy wyjściu, także po błędzie lub panice. Terminal po zamknięciu TUI działa tak jak przed jego uruchomieniem.
- R9. Gdy terminal nie obsługuje trybu wklejania, TUI działa jak dziś. Ten dokument nie wymaga obejścia dla takich terminali.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R4.** Given formularz nowego zadania z kursorem w polu promptu, when operator wkleja treść z trzema liniami, then pole zawiera trzy linie, formularz nie jest wysłany, a kursor jest na końcu trzeciej linii.
- AE2. **Covers R4.** Given treść wklejona jak w AE1, when operator poprawia drugą linię i naciska `Enter`, then formularz wysyła się z poprawioną treścią.
- AE3. **Covers R2.** Given treść skopiowana z systemu Windows z końcami linii `\r\n`, when operator wkleja ją do pola promptu, then każda linia źródła to jedna linia w polu, bez pustych linii pomiędzy.
- AE4. **Covers R3.** Given pole jednoliniowe (np. `ticket_id`), when operator wkleja `ABC-123\n`, then pole zawiera `ABC-123 ` i formularz nie jest wysłany.
- AE5. **Covers R6.** Given panel odpowiedzi na pytanie kroku, when operator wkleja treść z kilkoma liniami, then odpowiedź nie jest wysłana, a treść jest w polu do edycji.
- AE6. **Covers R7.** Given fokus na liście zadań, when operator wkleja treść zawierającą `q` i znaki nowej linii, then TUI nie zamyka się i nie otwiera żadnego zadania.

---

## Success Criteria

- Operator wkleja wieloliniowy opis zadania i zawsze może go sprawdzić i poprawić przed wysłaniem.
- Wklejenie nigdy nie wysyła formularza i nie wywołuje akcji w innym widoku.
- Planowanie nie musi zgadywać zachowania dla pól jedno- i wieloliniowych ani dla wklejenia poza polem tekstowym.

---

## Scope Boundaries

- Zmiana znaczenia `Enter` (nadal wysyła) i klawiszy nowej linii — bez zmian względem dokumentu z 2026-10-09.
- Obejście dla terminali bez trybu wklejania (np. wykrywanie wklejenia po szybkości klawiszy) — odłożone. Dodać tylko, gdy operator zgłosi taki terminal.
- Potwierdzenie przed wysłaniem lub podgląd wklejonej treści — nie w tym zakresie; R1–R4 wystarczą, bo operator i tak wysyła ręcznie.
- Cofanie wklejenia (undo) — nie w tym zakresie.
- Limit długości wklejonej treści i ostrzeżenia o dużej treści — nie w tym zakresie.

---

## Key Decisions

- Wklejenie rozpoznane przez tryb wklejania terminala zamiast heurystyki czasowej: zachowanie jest jednoznaczne i nie zależy od szybkości terminala ani od obciążenia systemu.
- W polu jednoliniowym nowa linia staje się spacją zamiast odrzucenia treści lub obcięcia do pierwszej linii: operator nie traci treści i od razu ją widzi.
- Wklejenie poza polem tekstowym jest ignorowane zamiast traktowane jako klawisze: przypadkowe wklejenie nie może wywołać akcji (np. wyjścia z TUI).

---

## Dependencies / Assumptions

- Terminale i multipleksery operatorów (np. tmux) obsługują tryb wklejania. Założenie niezweryfikowane na konkretnych terminalach operatorów; R9 opisuje zachowanie, gdy wsparcia nie ma.
- Obsługa trybu wklejania przez bibliotekę terminala na Windows jest niepewna (niezweryfikowane). Planowanie powinno to sprawdzić, jeżeli TUI ma działać na Windows.

---

## Document Review (2026-10-17, ce-doc-review, runda 1)

Typ dokumentu: requirements. Treść dokumentu powyżej nie jest zmieniona. Poprawki są tylko zalecane.

### Ustalenia

- **DR1. [P2, safe_auto, coherence, pewność 0.8] Summary mówi o nowych liniach także dla pól jednoliniowych.**
  Summary: „wstawia całą treść do pola, razem ze znakami nowej linii”. R3 i AE4 mówią, że w polu jednoliniowym nowa linia staje się spacją. Czytelnik Summary może oczekiwać, że nowa linia zostaje zawsze.
  Zalecana poprawka: „Wklejenie treści z wieloma liniami do pola tekstowego TUI wstawia całą treść do pola. W polu wieloliniowym znaki nowej linii zostają jako nowe linie, w polu jednoliniowym stają się spacjami. Wklejenie nie zatwierdza formularza.”

- **DR2. [P1, manual, coherence + scope-guardian + adversarial, pewność 0.75] Success Criteria mówi „zawsze” i „nigdy”, a R9 dopuszcza terminale bez trybu wklejania.**
  Według R9 na takim terminalu wklejenie nadal wysyła formularz po pierwszej linii, a reszta treści trafia jako klawisze do następnego widoku (AE6 może nadal wystąpić). TUI nie może wykryć, czy terminal obsługuje tryb wklejania. Wsparcie terminali operatorów (także tmux, ssh) jest niezweryfikowane. Oba kryteria obiecują więcej, niż daje zakres.
  Zalecenie: dodać na początku obu kryteriów „Gdy terminal obsługuje tryb wklejania, …”, albo dodać minimalne zabezpieczenie dla R9. Przed planowaniem sprawdzić tryb wklejania na terminalach operatorów.

- **DR3. [P2, manual, scope-guardian, pewność 0.65] R6 „wszystkie pola” nie ma zamkniętej listy pól.**
  W `src/tui/` pola tekstowe są w kilku modułach (np. `new_task.rs`, `failure_form.rs`, `waiting.rs`, `chooser.rs`, zakładka Dyrygent). AE1, AE4 i AE5 sprawdzają tylko część z nich. Ryzyko: jedno pole zostaje pominięte i nadal wysyła formularz po pierwszej linii.
  Zalecenie: dodać zamkniętą listę pól (albo wymóg, że planowanie ją sporządzi). Dodać przykład akceptacji dla zakładki Dyrygent i formularza błędu.

- **DR4. [P2, manual, feasibility + scope-guardian + design-lens + adversarial + coherence, pewność 0.7] R5 nie określa zachowania tabulatora i znaków sterujących.**
  R5 mówi „tak samo jak przy wpisywaniu z klawiatury”, ale z klawiatury `Tab` nie wstawia `\t`. `Tab` zmienia fokus (`src/tui/chooser.rs:724-734`), a `ChooserField::edit` wstawia tylko `KeyCode::Char` (`src/tui/chooser.rs:216-246`). Wymaganie jest więc wewnętrznie niespójne. Rysowanie pola (`sanitize`, `src/tui/scroll.rs:141-163`) zmienia `\t` na spacje, więc pozycja kursora może nie pasować do kolumny na ekranie. Fragmenty logów często mają sekwencje ANSI (`ESC[...m`). Ekran jest chroniony przez `sanitize`, ale surowe sekwencje trafiają do wartości wysyłanej do serwera.
  Zalecenie: określić, czy tabulator zostaje znakiem `\t`, czy staje się spacjami. Usuwać znaki sterujące C0/C1 i sekwencje ESC z wklejonej treści, oprócz nowych linii (R2, R3) i tabulatora.

- **DR5. [P2, manual, design-lens + scope-guardian, pewność 0.6] Brak decyzji o wyglądzie i przewijaniu pola po dużym wklejeniu.**
  Problem Frame wymienia „fragment logu”, który może mieć setki linii. R4 wymaga kursora na końcu, ale nie mówi, czy widok przewija się do kursora ani jak pole pokazuje treść poza widocznym obszarem. Limit długości jest odłożony bez uzasadnienia i bez sygnału do powrotu.
  Zalecenie: dodać wymaganie „po wklejeniu widok pola przewija się do kursora”. Określić wskaźnik przewijania lub licznik linii. Dodać uzasadnienie odłożenia limitu i sygnał, który przywraca go do zakresu.

- **DR6. [P2, manual, design-lens, pewność 0.7] R7 nie daje operatorowi informacji o zignorowanym wklejeniu.**
  Operator wkleja treść, a nic się nie dzieje. Nie wie, czy TUI działa i czy treść jest w polu.
  Zalecenie: zdecydować, czy TUI pokazuje krótki komunikat w linii statusu (np. „Wklejenie zignorowane: brak aktywnego pola”), czy ignoruje po cichu. Zapisać decyzję w Key Decisions.

- **DR7. [P2, manual, design-lens, pewność 0.55] Brak zachowania dla wklejenia przy zaznaczonym tekście.**
  R1 mówi „w pozycji kursora”. Nie wiadomo, czy wklejenie zastępuje zaznaczenie.
  Zalecenie: dodać wymaganie (zastępuje zaznaczenie) albo zapisać, że pola TUI nie mają zaznaczenia.

- **DR8. [P2, gated_auto, adversarial + design-lens + feasibility, pewność 0.65] Pole jednoliniowe: końcowa nowa linia staje się niewidoczną spacją.**
  Skopiowany identyfikator często kończy się nową linią. AE4 daje `ABC-123 ` z końcową spacją. Spacji nie widać, a wartość może nie przejść walidacji `ticket_id`. Uzasadnienie w Key Decisions („od razu ją widzi”) nie działa dla białych znaków na końcu. Dokument nie mówi też, czy `\r\n` w polu jednoliniowym to jedna spacja (R2 opisuje tylko pole wieloliniowe).
  Zalecenie: usuwać nowe linie z początku i końca wklejonej treści, a na spacje zamieniać tylko nowe linie w środku. Zapisać, że `\r\n` i `\r` dają jedną spację. Zmienić AE4: wynik `ABC-123`.

- **DR9. [P3, manual, design-lens + scope-guardian, pewność 0.5] Brak przypadków brzegowych fokusu.**
  Dokument nie opisuje wklejenia do pola wyłączonego lub tylko do odczytu, w trakcie wysyłania formularza ani przy otwartym dialogu lub potwierdzeniu.
  Zalecenie: dodać regułę „wklejenie trafia do pola tylko, gdy pole jest aktywne i edytowalne; w innym przypadku obowiązuje R7”.

- **DR10. [P3, manual, design-lens, pewność 0.5] R9 nie daje operatorowi wskazówki na terminalu bez trybu wklejania.**
  Na takim terminalu błąd z Problem Frame zostaje, a operator nie wie, że ma użyć `Ctrl+J`.
  Zalecenie: dodać wpis w pomocy lub README albo zapisać brak wskazówki jako świadomą decyzję.

- **DR11. [P1, gated_auto, adversarial + scope-guardian, pewność 0.6] Błąd włączenia trybu wklejania może zablokować start TUI. Nie wiadomo, czy Windows jest w zakresie.**
  Według reviewerów adversarial i feasibility polecenie włączenia trybu wklejania może zwrócić błąd (np. na konsoli Windows). Kod startowy w `src/tui/mod.rs` przekazuje błędy niektórych poleceń terminala dalej (`?`). Gdy włączenie trybu wklejania jest dodane w ten sposób, TUI nie startuje, co przeczy R9 („działa jak dziś”). Wyłączenie trybu trzeba dodać do `restore_terminal` (`src/tui/mod.rs:115-123`), czyli do wspólnej ścieżki dla `Drop` i panic hook. Feasibility ocenia, że crossterm na Windows najpewniej nie generuje zdarzeń wklejenia (niezweryfikowane w źródle crossterm). Dependencies zostawia zakres Windows planowaniu.
  Zalecenie: dodać do R8: „Błąd włączenia trybu wklejania nie przerywa startu TUI”. Zapisać w Scope Boundaries, czy Windows jest w zakresie.

- **DR12. [P2, manual, feasibility + adversarial, pewność 0.75] Brak zachowania dla pól liczbowych i pól wyboru.**
  Formularze mają pola `FieldKind::Integer` i `FieldKind::Select` (`src/tui/chooser.rs:71-81`, `src/tui/input_form.rs:996-1010`). Z klawiatury pole liczbowe przyjmuje tylko cyfry i `-` (`src/tui/chooser.rs:227-231`). Dokument dzieli pola tylko na „tekstowe” i „poza polem”. Planowanie musi zgadywać, co robi wklejenie `42\n` do pola liczbowego. To przeczy trzeciemu kryterium sukcesu.
  Zalecenie: dodać wymaganie, np. „R3a. W polu liczbowym wklejenie wstawia tylko znaki dozwolone z klawiatury (cyfry i `-`). W polu wyboru wklejenie jest ignorowane, jak w R7.”

### Ryzyka rezydualne

- Brak planu awaryjnego dla terminali bez trybu wklejania. To jest świadoma decyzja (R9), ale planowanie powinno sprawdzić terminale i multipleksery operatorów (np. tmux).
- R8 wymaga przywrócenia terminala także po panice. Dokument nie mówi, czy obecny kod już to robi.
- Duże wklejenie może spowolnić renderowanie pola. Dokument nie opisuje oczekiwanej responsywności.
- tmux wysyła bufor w trybie wklejania tylko z `paste-buffer -p`. Mosh i niektóre klienty SSH lub konsole webowe mogą nie przekazywać sekwencji trybu wklejania. Wtedy obowiązuje R9 mimo terminala, który ma wsparcie.

### Otwarte pytania

- Czy pola tekstowe TUI mają zaznaczenie tekstu?
- Czy wklejenie ma być jedną operacją dla przyszłej funkcji cofania (undo)?
- Czy R7 obejmuje okna dialogowe i potwierdzenia? Gdy otwarta jest pomoc (`help_open`) albo potwierdzenie wyjścia (`quit_confirm`, `src/tui/task_app/mod.rs:663-690`), czy wklejenie zamyka ten stan, czy jest całkowicie ignorowane?
- Czy TUI ma oficjalnie działać na Windows (conhost lub Windows Terminal)?

### Pokrycie

- ce-coherence-reviewer: zakończony.
- ce-design-lens-reviewer: zakończony.
- ce-scope-guardian-reviewer: zakończony. Jedno ustalenie miało nieprawidłowy format (puste pola) i jest odrzucone.
- ce-adversarial-document-reviewer: zakończony.
- ce-feasibility-reviewer: zakończony. Odniesienia do kodu w DR4, DR11 i DR12 pochodzą od reviewerów i nie są ponownie sprawdzone przez orkiestratora.
- Pliki referencyjne skilla (szablon, schemat ustaleń, zasady syntezy) były niedostępne z powodu uprawnień. Synteza jest wykonana na podstawie zasad z SKILL.md. Poprawki `safe_auto` nie są zastosowane w treści dokumentu, bo zadanie wymaga tylko zapisu przeglądu.
