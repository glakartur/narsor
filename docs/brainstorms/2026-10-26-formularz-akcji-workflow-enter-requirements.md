---
date: 2026-10-26
topic: formularz-akcji-workflow-enter
---

# Formularz wyboru akcji workflow: `Enter` nie zatwierdza po dłuższym oczekiwaniu

## Summary

Znajdź przyczynę, dla której `Enter` w formularzu wyboru akcji workflow (panel na dole szczegółów taska) czasem nie zatwierdza formularza, głównie po długim czekaniu. Potem usuń przyczynę tak, aby pierwszy `Enter` zawsze działał albo operator widział, dlaczego nie działa.

---

## Problem Frame

Operator otwiera formularz akcji workflow i czeka z decyzją. Po dłuższym czasie naciska `Enter`, a formularz się nie zatwierdza. Operator musi nacisnąć `Enter` kilka razy. Zatwierdzenie udaje się dopiero wtedy, gdy ostatnie pole i cały formularz zostaną zatwierdzone bardzo szybko, jeden `Enter` po drugim.

Artur podejrzewa asynchroniczne dostarczenie danych formularza do backendu. Nie zostało to jeszcze potwierdzone ani odrzucone.

Operator nie wie, czy `Enter` został przyjęty. Nie ma żadnego komunikatu. Ryzyko: operator myśli, że akcja poszła, albo traci zaufanie do formularza.

---

## Stan wiedzy z kodu (przegląd statyczny, bez uruchamiania)

Przyczyna **nie jest jeszcze potwierdzona**. Poniższe hipotezy wynikają z czytania kodu. Każda wymaga sprawdzenia w planie, zanim powstanie poprawka.

Ścieżka `Enter` w formularzu akcji workflow: `src/tui/task_app/panel/app.rs` (`on_panel_key`) → `src/tui/task_app/panel/state.rs` (`on_forms_key`, `sync`, `sync_forms`) → `src/tui/chooser.rs` (`on_fields_key`, `advance_field_or_submit`, `submit`) → `invoke` w `src/tui/task_app/actions.rs`.

- **H1. Odświeżenie zmienia stan formularza między dwoma `Enter`.** `sync_waiting` działa po każdym odczycie listy i po każdym zdarzeniu ze strumienia. Gdy aktywny formularz serwera się zmieni, `sync` wywołuje `leave_all`, które zabiera focus z pól. Kolejny `Enter` działa wtedy na liście akcji (otwiera pola), a nie na ostatnim polu. Przy długim oczekiwaniu przychodzi więcej zdarzeń, więc zmiana jest bardziej prawdopodobna. Pasuje do objawu „szybki `Enter` po `Enter` działa".
- **H2. Przebudowa formularza zamyka okno potwierdzenia.** `Chooser::rebuilt` wywołuje `decline()`: otwarte okno potwierdzenia znika po każdym `sync_forms`. Dotyczy formularzy z potwierdzeniem (`confirming_submit`). Formularz akcji „ręcznych" (`Slot::Manual`) w kodzie nie ma `confirming_submit`, więc H2 może go nie dotyczyć. Do sprawdzenia, który z formularzy to jest u Artura.
- **H3. Zapis jest czekany w pętli klawiszy.** Pętla TUI wykonuje zapis (`dispatch(...).await`) w gałęzi klawisza. Jeśli żądanie do backendu wisi (np. martwe połączenie po długiej bezczynności), pętla nie rysuje ekranu i nie obsługuje innych zdarzeń do końca żądania. To jest zgodne z intuicją Artura o asynchroniczności, ale wymaga pomiaru czasu odpowiedzi backendu.
- **H4. Blokada akcji zmieniających stan.** `mutation_allowed` odrzuca akcję, gdy połączenie nie jest `live` (strumień zerwany, ponawianie). Powód pojawia się tylko jako krótkie powiadomienie na pasku. Operator może go nie zauważyć. Po odzyskaniu połączenia pierwszy `Enter` działa od razu.

Hipotezy nie wykluczają się. Możliwe, że objaw ma więcej niż jedną przyczynę.

---

## Requirements

**Diagnoza**
- R1. Przed poprawką powstaje odtworzenie problemu: kroki, które w kontrolowanych warunkach dają objaw (długie oczekiwanie symulowane zdarzeniem lub odświeżeniem, opóźnienie lub zerwanie połączenia z backendem).
- R2. Dla każdej z hipotez H1–H4 jest wynik: potwierdzona albo odrzucona, z dowodem (test, log, pomiar). Wynik trafia do planu i do opisu poprawki.
- R3. Diagnoza ustala, który formularz widzi operator: akcje workflow „ręczne" czy formularz wpisu z potwierdzeniem. To zawęża hipotezy.

**Zachowanie po poprawce**
- R4. Gdy operator wypełnił formularz i naciska `Enter` na ostatnim polu, formularz zostaje zatwierdzony przy pierwszym `Enter`, niezależnie od tego, ile czasu minęło od otwarcia i ile zdarzeń przyszło w tym czasie.
- R5. Odświeżenie danych (odczyt listy, zdarzenie ze strumienia) nie zabiera focusa z pola, nie zamyka okna potwierdzenia i nie kasuje wpisanego tekstu, dopóki ten sam formularz nadal jest dostępny.
- R6. Gdy formularz naprawdę znika albo backend wskazuje inny domyślny formularz, panel przełącza się zgodnie z istniejącą regułą z `docs/brainstorms/2026-10-25-tui-panel-rozmowy-trzy-formularze-requirements.md` (R13, R14). Operator widzi komunikat o zmianie, a szkic zostaje.
- R7. Gdy `Enter` nie może zatwierdzić formularza (brak połączenia, zapis w toku, błąd walidacji), operator widzi to od razu i wyraźnie, w miejscu panelu. Ciche zignorowanie `Enter` nie jest dopuszczalne.
- R8. Gdy zapis trwa, `Enter` nie wysyła formularza drugi raz. Klucz idempotencji istniejącej ścieżki zostaje zachowany (jedna decyzja = jedno wykonanie).
- R9. Pętla TUI nie blokuje obsługi klawiszy i rysowania na czas zapisu do backendu, albo operator widzi stan „wysyłam…". (Zakres zależy od wyniku H3.)

---

## Acceptance Examples

- AE1. **Covers R4, R5.** Dane: formularz akcji otwarty i wypełniony, focus w ostatnim polu. W tle przychodzi zdarzenie, które powoduje odświeżenie listy, ale formularz nadal jest dostępny. Gdy operator naciska `Enter`, formularz zostaje zatwierdzony za pierwszym razem.
- AE2. **Covers R5.** Dane: okno potwierdzenia jest otwarte. Przychodzi odświeżenie listy. Okno zostaje otwarte, a `Enter` je zatwierdza.
- AE3. **Covers R7.** Dane: połączenie z backendem jest zerwane. Gdy operator naciska `Enter` na ostatnim polu, panel pokazuje powód odmowy. Wpisany tekst zostaje.
- AE4. **Covers R8.** Dane: zapis formularza trwa. Gdy operator naciska `Enter` ponownie, żądanie nie jest wysłane drugi raz.
- AE5. **Covers R6.** Dane: agent zadał pytanie w czasie, gdy operator oglądał formularz akcji. Panel przełącza się na pytanie, pokazuje komunikat, a szkic formularza akcji zostaje.

---

## Success Criteria

- Operator zatwierdza formularz akcji pierwszym `Enter` po dowolnie długim oczekiwaniu. Artur nie odtwarza już objawu.
- Gdy `Enter` nie działa, operator wie dlaczego.
- Planista ma udokumentowaną, potwierdzoną przyczynę (nie tylko hipotezę) i test regresji, który ją pokrywa.

---

## Scope Boundaries

- Brak zmian w układzie wizualnym formularzy, w skrótach klawiszy i w regułach `Esc` / `Tab`.
- Brak zmian kontraktu API z backendem. Jeśli diagnoza pokaże problem po stronie serwera (czas odpowiedzi, utrata połączenia), ten kawałek jest osobnym zadaniem.
- Brak ogólnego przepisania pętli zdarzeń TUI. Zmiana dotyczy tylko tego, co potrzebne do R4–R9.

---

## Key Decisions

- Najpierw diagnoza, potem poprawka. Hipotezy są z przeglądu kodu i nie wolno ich traktować jako faktów.
- Wymagania opisują zachowanie, które operator ma widzieć, a nie wybraną poprawkę. Wybór poprawki (np. czy zachować focus przy `sync`, czy przenieść zapis poza pętlę klawiszy) należy do planu, po wyniku diagnozy.

---

## Założenia do potwierdzenia

- Z1. „Formularz wyboru akcji workflow" to panel akcji na dole szczegółów taska (typ „akcja workflow" z `2026-10-25-tui-panel-rozmowy-trzy-formularze-requirements.md`), a nie formularz nowego zadania.
- Z2. „Dłuższy czas oczekiwania" to minuty, w których TUI działa i dostaje zdarzenia (nie zamknięcie i ponowne uruchomienie TUI).
- Z3. Objaw występuje z lokalnym backendem (daemon na tej samej maszynie) tak samo jak ze zdalnym. Jeśli tylko ze zdalnym, H3 i H4 stają się główne.
- Z4. Przy `Enter`, który „nie działa", ekran nadal się rysuje (np. kursor miga, spinner działa). Gdyby ekran stał, to wskazuje na H3.
- Z5. Wybrana jest wersja bezpieczna: najpierw diagnoza w planie, bez zgadywania poprawki.

---

## Pytania otwarte dla użytkownika

- Q1. Czy podczas objawu na pasku pojawia się jakikolwiek komunikat (np. „ponawiam połączenie", „akcja zmieniająca stan jest zablokowana")? To rozstrzyga H4.
- Q2. Czy w czasie objawu ekran nadal reaguje (przewijanie, spinner), czy wygląda na zawieszony? To rozstrzyga H3.
- Q3. Czy po `Enter`, który „nie działa", pojawia się okno potwierdzenia, które potem znika? To wskazuje H2.
- Q4. Czy objaw występuje także w innych formularzach panelu (odpowiedź na pytanie agenta, rozmowa), czy tylko w formularzu akcji workflow?
- Q5. Z jakim backendem pracuje TUI w chwili objawu: lokalny daemon czy zdalny serwer?

---

## Outstanding Questions

### Resolve Before Planning

- Brak. Pytania Q1–Q5 zawężają diagnozę, ale plan może zacząć od odtworzenia (R1) bez nich.

### Deferred to Planning

- [Affects R1][Needs research] Jak odtworzyć w teście „długie oczekiwanie": wstrzyknięcie zdarzenia i `sync` między dwoma `Enter` w teście `TaskApp`, plus opóźnienie klienta HTTP.
- [Affects R5][Technical] Czy `sync` powinien oddawać focus polom po zmianie aktywnego formularza, tylko gdy to ten sam formularz.
- [Affects R9][Technical] Czy zapis (`dispatch(...).await` w gałęzi klawisza) przenieść do zadania w tle, jak odczyty.
- [Affects R7][Technical] Miejsce i forma komunikatu o odmowie w panelu.
