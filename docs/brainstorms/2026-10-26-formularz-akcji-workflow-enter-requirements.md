---
date: 2026-10-26
topic: formularz-akcji-workflow-enter
---

# Formularz wyboru akcji workflow: `Enter` nie zatwierdza po kilkunastu sekundach oczekiwania

## Summary

Znajdź przyczynę, dla której `Enter` w formularzu wyboru akcji workflow (panel na dole szczegółów taska) czasem nie zatwierdza formularza, głównie po kilkunastu sekundach otwartego formularza. Potem usuń przyczynę tak, aby pierwszy `Enter` zawsze działał albo operator widział, dlaczego nie działa.

---

## Problem Frame

Operator otwiera formularz akcji workflow i czeka z decyzją kilkanaście sekund. Potem naciska `Enter`. Pojawia się okno potwierdzenia zakończenia pracy z formularzem, ale `Enter` nie zamyka formularza. Operator nadal może wybierać opcje. Czasem po chwili lista dostępnych opcji się zmienia. Operator musi nacisnąć `Enter` kilka razy. Zatwierdzenie udaje się dopiero wtedy, gdy ostatnie pole i cały formularz zostaną zatwierdzone bardzo szybko, jeden `Enter` po drugim.

Ekran w tym czasie reaguje na klawisze. Artur nie widział komunikatu na pasku. Objaw wystąpił z lokalnym backendem. Artur nie zauważył go przy dodawaniu nowych tasków.

Artur podejrzewa asynchroniczne dostarczenie danych formularza do backendu. Nie zostało to jeszcze potwierdzone ani odrzucone.

Operator nie wie, czy `Enter` został przyjęty. Nie ma żadnego komunikatu. Ryzyko: operator myśli, że akcja poszła, albo traci zaufanie do formularza.

---

## Stan wiedzy z kodu (przegląd statyczny, bez uruchamiania)

Przyczyna **nie jest jeszcze potwierdzona**. Poniższe hipotezy wynikają z czytania kodu. Każda wymaga sprawdzenia w planie, zanim powstanie poprawka.

Ścieżka `Enter` w formularzu akcji workflow: `src/tui/task_app/panel/app.rs` (`on_panel_key`) → `src/tui/task_app/panel/state.rs` (`on_forms_key`, `sync`, `sync_forms`) → `src/tui/chooser.rs` (`on_fields_key`, `advance_field_or_submit`, `submit`) → `invoke` w `src/tui/task_app/actions.rs`.

- **H1. Odświeżenie zmienia stan formularza między dwoma `Enter`.** `sync_waiting` działa po każdym odczycie listy i po każdym zdarzeniu ze strumienia. Gdy aktywny formularz serwera się zmieni, `sync` wywołuje `leave_all`, które zabiera focus z pól. Kolejny `Enter` działa wtedy na liście akcji (otwiera pola), a nie na ostatnim polu. Im dłużej formularz jest otwarty, tym więcej zdarzeń przychodzi, więc zmiana jest bardziej prawdopodobna. Pasuje do objawu „szybki `Enter` po `Enter` działa".
- **H2. Przebudowa formularza zamyka okno potwierdzenia.** `Chooser::rebuilt` wywołuje `decline()`: otwarte okno potwierdzenia znika po każdym `sync_forms`. Dotyczy formularzy z potwierdzeniem (`confirming_submit`). Formularz akcji „ręcznych" (`Slot::Manual`) w kodzie nie ma `confirming_submit`, więc H2 może go nie dotyczyć. Artur widzi okno potwierdzenia (odpowiedź Q3), więc najpewniej to formularz z `confirming_submit` i H2 jest główną hipotezą. Do potwierdzenia w diagnozie (R3).
- **H3. Zapis jest czekany w pętli klawiszy.** Pętla TUI wykonuje zapis (`dispatch(...).await`) w gałęzi klawisza. Jeśli żądanie do backendu wisi, pętla nie rysuje ekranu i nie obsługuje innych zdarzeń do końca żądania. Artur zgłosił, że ekran reaguje (Q2), a backend jest lokalny (Q5). To osłabia H3. Wymaga pomiaru czasu odpowiedzi backendu, aby ją odrzucić.
- **H4. Blokada akcji zmieniających stan.** `mutation_allowed` odrzuca akcję, gdy połączenie nie jest `live` (strumień zerwany, ponawianie). Powód pojawia się tylko jako krótkie powiadomienie na pasku. Operator może go nie zauważyć. Po odzyskaniu połączenia pierwszy `Enter` działa od razu. Artur nie zauważył komunikatu (Q1), a backend jest lokalny (Q5). To osłabia H4, ale nie wyklucza jej, bo komunikat jest krótki.

Hipotezy nie wykluczają się. Możliwe, że objaw ma więcej niż jedną przyczynę.

---

## Requirements

**Diagnoza**
- R1. Przed poprawką powstaje odtworzenie problemu: kroki, które w kontrolowanych warunkach dają objaw (długie oczekiwanie symulowane zdarzeniem lub odświeżeniem, opóźnienie lub zerwanie połączenia z backendem).
- R2. Dla każdej z hipotez H1–H4 jest wynik: potwierdzona albo odrzucona, z dowodem (test, log, pomiar). Wynik trafia do planu i do opisu poprawki.
- R3. Diagnoza potwierdza, który formularz widzi operator. Odpowiedź Q3 wskazuje formularz z oknem potwierdzenia. To zawęża hipotezy do H2 i H1.

**Zachowanie po poprawce**
- R4. Obecny przebieg zostaje bez zmian: `Enter` przechodzi między polami, po ostatnim polu pojawia się okno potwierdzenia wysyłki, `Enter` w oknie zatwierdza, `Esc` odrzuca. Gdy okno potwierdzenia jest otwarte, pierwszy `Enter` zatwierdza formularz, niezależnie od tego, ile czasu minęło od otwarcia formularza i ile zdarzeń przyszło w tym czasie.
- R5. Odświeżenie danych (odczyt listy, zdarzenie ze strumienia) nie zabiera focusa z pola, nie zamyka okna potwierdzenia i nie kasuje wpisanego tekstu, dopóki ten sam formularz nadal jest dostępny.
- R6. Gdy formularz naprawdę znika albo backend wskazuje inny domyślny formularz, panel przełącza się zgodnie z istniejącą regułą z `docs/brainstorms/2026-10-25-tui-panel-rozmowy-trzy-formularze-requirements.md` (R13, R14). Operator widzi komunikat o zmianie, a szkic zostaje.
- R7. Gdy `Enter` nie może zatwierdzić formularza (brak połączenia, zapis w toku, błąd walidacji), operator widzi to od razu i wyraźnie, w miejscu panelu. Ciche zignorowanie `Enter` nie jest dopuszczalne.
- R8. Gdy zapis trwa, `Enter` nie wysyła formularza drugi raz. Klucz idempotencji istniejącej ścieżki zostaje zachowany (jedna decyzja = jedno wykonanie).
- R9. Pętla TUI nie blokuje obsługi klawiszy i rysowania na czas zapisu do backendu, albo operator widzi stan „wysyłam…". (Zakres zależy od wyniku H3.)

---

## Acceptance Examples

- AE1. **Covers R4, R5.** Dane: formularz akcji otwarty i wypełniony, okno potwierdzenia wysyłki jest otwarte. W tle przychodzi zdarzenie, które powoduje odświeżenie listy, ale formularz nadal jest dostępny. Gdy operator naciska `Enter`, formularz zostaje zatwierdzony za pierwszym razem.
- AE2. **Covers R5.** Dane: okno potwierdzenia jest otwarte. Przychodzi odświeżenie listy. Okno zostaje otwarte, a `Enter` je zatwierdza.
- AE3. **Covers R7.** Dane: połączenie z backendem jest zerwane. Gdy operator naciska `Enter` na ostatnim polu, panel pokazuje powód odmowy. Wpisany tekst zostaje.
- AE4. **Covers R8.** Dane: zapis formularza trwa. Gdy operator naciska `Enter` ponownie, żądanie nie jest wysłane drugi raz.
- AE5. **Covers R6.** Dane: agent zadał pytanie w czasie, gdy operator oglądał formularz akcji. Panel przełącza się na pytanie, pokazuje komunikat, a szkic formularza akcji zostaje.

---

## Success Criteria

- Operator zatwierdza formularz akcji pierwszym `Enter` w oknie potwierdzenia po dowolnie długim oczekiwaniu. Artur nie odtwarza już objawu.
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

## Założenia (potwierdzone przez Artura)

- Z1. Potwierdzone. „Formularz wyboru akcji workflow” to panel akcji na dole szczegółów taska (typ „akcja workflow” z `2026-10-25-tui-panel-rozmowy-trzy-formularze-requirements.md`), a nie formularz nowego zadania.
- Z2. Skorygowane. „Dłuższy czas oczekiwania” to kilkanaście sekund otwartego formularza, a nie minuty.
- Z3. Zawężone. Objaw występuje z lokalnym backendem (daemon na tej samej maszynie). Zdalny backend nie jest potrzebny do objawu, więc H3 i H4 nie są główne.
- Z4. Potwierdzone. Ekran w czasie objawu reaguje. To wskazuje przeciw H3.
- Z5. Wybrana jest wersja bezpieczna: najpierw diagnoza w planie, bez zgadywania poprawki.

---

## Odpowiedzi na pytania

- Q1. Artur nie zauważył komunikatu na pasku. H4 nie jest potwierdzona. Brak komunikatu jest zgodny z R7: komunikat musi być widoczny w panelu.
- Q2. Cały ekran reaguje. H3 jako zawieszenie pętli jest mało prawdopodobna.
- Q3. Tak, ale dokładniej: `Enter` dotyczy okna potwierdzenia zakończenia pracy z formularzem. Po `Enter` okno nie zamyka formularza, operator nadal wybiera opcje, a lista opcji czasem zmienia się po chwili. To wskazuje H2 (przebudowa formularza zamyka okno potwierdzenia) i H1 (odświeżenie listy opcji).
- Q4. Artur nie zauważył objawu przy dodawaniu nowych tasków. Czy objaw dotyczy odpowiedzi na pytanie agenta lub rozmowy, nie wiadomo. Plan sprawdza to w odtworzeniu (R1).
- Q5. Lokalny backend.

---

## Outstanding Questions

### Resolve Before Planning

- Brak. Odpowiedzi Q1–Q5 są w sekcji „Odpowiedzi na pytania”. Plan zaczyna od odtworzenia (R1) na ścieżce okna potwierdzenia (H2, H1) z lokalnym backendem. Q4 zostaje do sprawdzenia w odtworzeniu.

### Deferred to Planning

- [Affects R1][Needs research] Jak odtworzyć w teście „długie oczekiwanie": wstrzyknięcie zdarzenia i `sync` między dwoma `Enter` w teście `TaskApp`, plus opóźnienie klienta HTTP.
- [Affects R5][Technical] Czy `sync` powinien oddawać focus polom po zmianie aktywnego formularza, tylko gdy to ten sam formularz.
- [Affects R9][Technical] Czy zapis (`dispatch(...).await` w gałęzi klawisza) przenieść do zadania w tle, jak odczyty.
- [Affects R7][Technical] Miejsce i forma komunikatu o odmowie w panelu.
