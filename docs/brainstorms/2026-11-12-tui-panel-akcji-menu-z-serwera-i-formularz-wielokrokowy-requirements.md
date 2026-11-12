---
date: 2026-11-12
topic: tui-panel-akcji-menu-z-serwera-i-formularz-wielokrokowy
---

# Panel akcji taska: menu tylko z dostępnych akcji serwera i formularz wielokrokowy

## Summary

Serwer zwraca tylko akcje dostępne teraz. Każda akcja ma opis w słowach dla operatora i informację, czy jest ostatecznym wyborem, czy otwiera podmenu. TUI nie ma własnej listy akcji: rysuje to, co dał serwer, na standardowych kontrolkach. Wybór akcji z dodatkowymi danymi przechodzi na kolejny krok formularza wielokrokowego, a każda akcja kończy się potwierdzeniem. Formularz wielokrokowy jest nową, wspólną kontrolką aplikacji.

---

## Problem Frame

Operator naciska `Ctrl+.` na tasku i widzi listę akcji. Część pozycji jest wyszarzona, bo teraz nie działa. Operator czyta powody, zamiast wybierać. Lista jest długa, a użyteczne są dwie lub trzy pozycje.

Przyczyna jest w podziale pracy między serwerem a TUI:

- Serwer zwraca pełny zestaw akcji z flagą „dostępna" i powodem (`TaskActionOffer.available`, `reason` w `crates/narsor-protocol/src/controller/task/actions.rs`, `offer` w `crates/narsor-engine/src/controller_protocol/service/tasks/actions.rs`).
- TUI ma własną, stałą listę akcji (`TaskAction::ALL`, `menu_items` w `src/tui/task_app/actions.rs`). Dla każdej pozycji zna nazwę i opis. Każda nowa akcja serwera wymaga zmiany w TUI.
- Klient sam wie, że „przejście ręczne" i „wybierz krok" otwierają podmenu, a „pauza" jest od razu wykonywana. Serwer tego nie mówi.

Obie strony robią więc coś, co należy do drugiej. Serwer powinien decydować, co jest dostępne, jak się nazywa i jak działa. TUI powinien to pokazać.

Drugi problem to forma. Akcje używają osobnych okien (`Overlay::Menu`, `Transitions`, `Steps`, `Comment`, `Confirm`). Każde ma własną ścieżkę zatwierdzania. Pole z treścią dla agenta jest w tym samym kroku co wybór. Operator nie ma spójnego wzorca „wybierz, uzupełnij, potwierdź". Ten sam brak dotyczy odpowiedzi na pytania agenta i dodawania nowego taska.

---

## Actors

- A1. Operator: osoba obsługująca task w TUI.
- A2. Serwer (control plane): zna stan taska. Podaje listę dostępnych akcji z opisami i rodzajem każdej. Odrzuca akcję, która przestała być dozwolona.
- A3. Klient (TUI): rysuje akcje serwera. Nie rozumie, co akcja znaczy.

---

## Key Flows

- F1. Akcja wykonywana od razu (liść)
  - **Trigger:** Operator naciska `Ctrl+.` na tasku i wybiera akcję rodzaju „wykonaj".
  - **Actors:** A1, A2, A3
  - **Steps:** Panel pokazuje tylko dostępne akcje. Operator wybiera jedną. Klient pokazuje krok potwierdzenia z opisem skutku. Operator zatwierdza. Klient wysyła akcję. Serwer ją przyjmuje albo odrzuca z powodem.
  - **Outcome:** Akcja została wykonana albo operator widzi powód odmowy.
  - **Covered by:** R1–R6, R12, R13

- F2. Akcja z podmenu
  - **Trigger:** Operator wybiera akcję rodzaju „podmenu" (na przykład wybór kroku workflow).
  - **Actors:** A1, A2, A3
  - **Steps:** Klient pokazuje następny poziom: listę opcji, które dał serwer. Operator wybiera opcję. Jeśli opcja jest liściem, wchodzi krok potwierdzenia. `Esc` wraca o jeden poziom.
  - **Outcome:** Operator wybrał konkretną akcję i ją potwierdził.
  - **Covered by:** R3, R4, R7, R8

- F3. Akcja z dodatkowymi danymi
  - **Trigger:** Wybrana opcja wymaga danych od operatora (na przykład komentarz dla agenta przy wyborze kroku).
  - **Actors:** A1, A2, A3
  - **Steps:** Klient pokazuje krok z polami, które opisał serwer. Operator wypełnia je. Klient pokazuje krok potwierdzenia z podsumowaniem: akcja i wpisane dane. Operator zatwierdza.
  - **Outcome:** Akcja wysłana z danymi. Dane z niezatwierdzonego kroku nie giną przy powrocie o krok.
  - **Covered by:** R9–R11

---

## Requirements

**Kontrakt serwera**

- R1. Serwer zwraca w liście akcji taska tylko akcje dostępne w chwili odczytu. Akcja niedostępna nie występuje w liście. Pole „dostępna" i powód niedostępności przestają być potrzebne klientowi do rysowania listy.
- R2. Każda akcja ma od serwera: stały identyfikator, nazwę i opis skutku w słowach dla operatora (po polsku), oznaczenie ryzyka (czy jest nieodwracalna) oraz kolejność. TUI nie ma własnych nazw ani opisów akcji taska.
- R3. Każda akcja ma od serwera rodzaj: „wykonaj" (ostateczny wybór) albo „podmenu" (ma w sobie opcje do wyboru). Podmenu niesie listę opcji, które mają ten sam kształt co akcja, więc mogą być liściem albo kolejnym podmenu.
- R4. Akcja rodzaju „wykonaj" może opisać pola, które operator uzupełnia przed wykonaniem: nazwa, rodzaj (tekst, tekst wieloliniowy, wybór z listy), wymagane lub nie, podpowiedź. Klient rysuje pola z tego opisu. Nie zna ich znaczenia.
- R5. Serwer nadal sprawdza akcję przy wysłaniu i odrzuca ją z powodem, gdy w międzyczasie przestała być dozwolona. Klient pokazuje powód i odświeża listę. (Zachowanie z `docs/brainstorms/2026-11-11-akcje-taska-pauza-stop-i-panel-akcji-requirements.md`, R2.)
- R6. Gdy serwer nie podał listy (starszy serwer, brak danych), panel pokazuje jasny komunikat „brak danych z serwera" i nie pokazuje żadnej akcji. Klient nie podstawia własnej listy.

**Panel w TUI**

- R7. Panel akcji rysuje listę z serwera na standardowej kontrolce listy z opisem (`Chooser`). Wysokość listy, przewijanie, podświetlenie i opis wybranej pozycji działają tak jak w innych miejscach aplikacji.
- R8. Akcja rodzaju „podmenu" otwiera kolejny poziom w tym samym panelu, nie nowe okno. `Esc` wraca o jeden poziom. `Esc` na pierwszym poziomie zamyka panel. Nagłówek panelu pokazuje ścieżkę (na przykład „Akcje › Wybierz krok").
- R9. Dane do uzupełnienia nie są w kroku wyboru. Są na osobnym, następnym kroku formularza wielokrokowego (R11).
- R10. Po wypełnieniu pól operator widzi krok potwierdzenia. Krok pokazuje nazwę akcji, jej opis skutku, wpisane dane i znak ryzyka, jeśli akcja jest nieodwracalna. To ten sam wzorzec co potwierdzenie wysyłki komunikatu do agenta. Wszystkie akcje wymagają potwierdzenia, także te bez pól. Wyjątkiem jest akcja, którą serwer oznaczył jako lokalną i bez skutku (na przykład otwarcie linku), jeśli taka zostanie w panelu.
- R11. Powstaje wspólna kontrolka formularza wielokrokowego. Opisuje ją lista kroków: wybór, pola, potwierdzenie. Kontrolka daje: wskaźnik kroku („krok 2 z 3"), przejście dalej (`Enter`), powrót (`Esc`) z zachowaniem wpisanych danych, pomijanie kroków, których nie ma, oraz jedno zdarzenie „zatwierdzono" na końcu. Używa istniejącego silnika formularza (`src/tui/form/`) i `Chooser`. Nie powiela jego reguł edycji.

**Miejsce i wygląd**

- R12. Panel akcji pokazuje się w miejscu formularza dodawania treści (panel rozmowy na dole szczegółów taska), a nie jako okno na środku ekranu. Potwierdzone przez operatora (Q1, odpowiedź 1). Gdy panel rozmowy nie jest widoczny (inna zakładka), panel akcji zajmuje to samo miejsce w układzie szczegółów.
- R13. Panel nie zmienia znaczenia skrótu `Ctrl+.` (ani zapasowego `.`). Po wykonaniu akcji, po odmowie serwera albo po `Esc` focus wraca tam, gdzie był przed otwarciem panelu. (Spójne z `docs/brainstorms/2026-10-19-focus-po-zatwierdzeniu-akcji-requirements.md`.)
- R14. Podczas otwartego panelu odświeżenie listy z serwera nie zamyka panelu, nie zabiera focusa i nie kasuje wpisanych danych, dopóki wybrana akcja nadal jest dostępna. Gdy znika, operator widzi komunikat z powodem i wraca na poprzedni poziom. (Ta sama zasada co R5 w `docs/brainstorms/2026-10-26-formularz-akcji-workflow-enter-requirements.md`.)

**Spójność i podział odpowiedzialności**

- R15. Reguły dostępności akcji są tylko po stronie serwera. TUI nie ma `match` po identyfikatorze akcji, który decyduje o nazwie, opisie, rodzaju lub dostępności. Dopuszczalny wyjątek: akcje z natury lokalne klienta (przesuwanie w kolejce, otwarcie linku), jeśli zostają poza panelem.
- R16. Klient nie zakłada znaczenia akcji, której nie zna. Nieznany identyfikator z poprawnym opisem (R2–R4) działa jak każdy inny: jest pokazany, wybrany, potwierdzony i wysłany.
- R17. Wiele miejsc aplikacji używa tej samej kontrolki wielokrokowej i tego samego wzorca „wybierz, uzupełnij, potwierdź". W tym zakresie tylko panel akcji przechodzi na nową kontrolkę (patrz Scope Boundaries). Kontrolka jest zaprojektowana tak, żeby odpowiedź na pytanie agenta i formularz nowego taska mogły z niej korzystać bez zmiany jej interfejsu.

---

## Acceptance Examples

- AE1. **Covers R1, R7.** Dane: task działa. Serwer ma akcje: pauza, stop, archiwizuj. Gdy operator naciska `Ctrl+.`, panel pokazuje dokładnie te trzy pozycje. Nie ma pozycji „zwolnij slot" ani „uruchom od nowa", nawet wyszarzonych.
- AE2. **Covers R3, R8.** Dane: task jest zatrzymany. Akcja „Wybierz krok" jest rodzaju „podmenu". Gdy operator ją wybiera, ten sam panel pokazuje listę kroków workflow z nagłówkiem „Akcje › Wybierz krok". `Esc` wraca do listy akcji.
- AE3. **Covers R4, R9, R10, R11.** Dane: operator wybrał krok agentowy w podmenu. Krok 2 pokazuje pole „komentarz dla agenta" (opcjonalne). Krok 3 pokazuje potwierdzenie z nazwą kroku i treścią komentarza. Gdy operator wraca `Esc` do kroku 2, komentarz jest nadal wpisany.
- AE4. **Covers R10.** Dane: operator wybrał „Pauza". Panel pokazuje krok potwierdzenia z opisem skutku. Dopiero `Enter` w tym kroku wysyła akcję. `Esc` nie wysyła niczego.
- AE5. **Covers R5.** Dane: operator potwierdził „Archiwizuj", a stan taska zmienił się tak, że serwer odrzuca akcję. Operator widzi powód. Panel odświeża listę.
- AE6. **Covers R6.** Dane: serwer nie podał listy akcji. Panel pokazuje „brak danych z serwera" i brak pozycji do wyboru.
- AE7. **Covers R14.** Dane: operator jest w kroku z polem i pisze komentarz. Przychodzi zdarzenie ze strumienia, które odświeża listę. Pole zachowuje tekst i focus.
- AE8. **Covers R16.** Dane: serwer dodał nową akcję „X" z nazwą, opisem i rodzajem „wykonaj". Stary klient, który nie zna „X", pokazuje ją i wykonuje po potwierdzeniu.

---

## Success Criteria

- Operator widzi w panelu tylko to, co może zrobić teraz. Nie czyta powodów niedostępności.
- Nowa akcja serwera pojawia się w TUI bez zmiany kodu TUI.
- Każda akcja ma ten sam przebieg: wybór, opcjonalne pola, potwierdzenie. Operator nie musi zgadywać, kiedy akcja się wykona.
- Kontrolka wielokrokowa jest gotowa do użycia przy odpowiedzi na pytania agenta i przy dodawaniu taska. Te dwa miejsca nie wymagają zmian jej interfejsu.
- Planista nie musi wymyślać, które akcje są podmenu, a które liśćmi, ani skąd bierze się opis. Odpowiada na to R1–R4.

---

## Scope Boundaries

- Ta zmiana nie przenosi odpowiedzi na pytania agenta ani formularza nowego taska na kontrolkę wielokrokową. Powstaje tylko kontrolka i jej pierwsze użycie w panelu akcji. Dwa kolejne przeniesienia to osobne prace.
- Brak nowych akcji ani zmian w regułach dostępności (pauza, stop, zwolnij slot, restart, wybór kroku, archiwizacja). Zmienia się sposób, w jaki serwer je opisuje, i sposób, w jaki TUI je rysuje.
- Brak zmiany skrótów `Ctrl+.` i `.` oraz skrótów `Ctrl+↑` / `Ctrl+↓` do przesuwania w kolejce.
- Brak zmian w panelu rozmowy poza miejscem, w którym panel akcji się pokazuje.
- Brak zmiany modelu uprawnień. Kto może wykonać akcję, określa dotychczasowa autoryzacja.
- Brak akcji grupowych na wielu taskach.

---

## Key Decisions

- Serwer zwraca tylko dostępne akcje: Dlaczego: operator nie czyta nieaktualnych pozycji, a klient nie ma czego filtrować. Powód niedostępności przy próbie wysłania zostaje w odpowiedzi serwera (R5).
- Serwer opisuje akcje całkowicie (nazwa, opis, rodzaj, pola, ryzyko): Dlaczego: klient nie musi znać znaczenia akcji. Kontrakt jest jedynym miejscem, które zmienia się z nową akcją.
- Wszystkie akcje mają krok potwierdzenia: Dlaczego: jeden wzorzec jest łatwiejszy do nauczenia niż wybór „ta pyta, ta nie". To ta sama zasada co przy wysyłce komunikatu do agenta. Koszt: jeden `Enter` więcej przy akcjach bezpiecznych. Potwierdzone przez operatora (Q2, odpowiedź 1).
- Formularz wielokrokowy jest kontrolką wspólną, a nie kodem panelu akcji: Dlaczego: ta sama potrzeba jest przy pytaniach agenta i nowym tasku. Kontrolka opisana listą kroków nie wie, czym jest akcja.
- Wyjątek dla akcji lokalnych klienta (przesuwanie w kolejce, otwarcie linku) zostaje: Dlaczego: serwer nie zna kolejki widzianej przez klienta ani przeglądarki operatora. Przesuwanie nie jest w panelu.

---

## Dependencies / Assumptions

- Zweryfikowane w kodzie: `TaskActionOffer` ma dziś `available`, `reason`, `effect` i `options` (etykieta, opis). Nie ma rodzaju akcji ani opisu pól. Serwer `offer()` zwraca wszystkie akcje z `OFFERED`, także niedostępne.
- Zweryfikowane w kodzie: TUI ma stałą listę `TaskAction::ALL` oraz `menu_text` z nazwami i opisami, i osobne nakładki `Menu`, `Transitions`, `Steps`, `Comment`, `Confirm` w `src/tui/task_app/actions.rs`.
- Zweryfikowane w kodzie: `Chooser` (`src/tui/chooser.rs`) ma pola, opis, `confirming_submit` i wsparcie dla układu z polami. Silnik formularza jest w `src/tui/form/`. Nie ma kontrolki wielokrokowej.
- Założenie: pole „dostępna" i `reason` zostają w kontrakcie dla zgodności ze starszymi klientami, ale serwer nie zwraca już niedostępnych pozycji. Starszy klient pokaże więc krótszą listę. Wymaga potwierdzenia w planowaniu, bo „brak akcji" starszy klient czyta jako „brak danych" (`NO_OFFER`).
- Założenie: nowy kontrakt jest dodatkowy (nowe pola z wartościami domyślnymi), tak jak dotychczasowe rozszerzenia protokołu.
- Niezweryfikowane: czy panel rozmowy ma dość miejsca, żeby pomieścić listę z opisem i kroki formularza w małym terminalu. Do sprawdzenia w planowaniu.

---

## Outstanding Questions

### Resolve Before Planning

Brak. Q1 i Q2 rozstrzygnięte przez operatora: panel akcji w miejscu formularza dodawania treści (R12), potwierdzenie dla wszystkich akcji (R10).

### Deferred to Planning

- [Affects R2–R4][Technical] Kształt nowych pól kontraktu i sposób zgodności ze starszymi klientami i serwerami.
- [Affects R11][Technical] Granica między kontrolką wielokrokową a istniejącym `Chooser` i `FormEngine`, tak by nie powstały dwa silniki.
- [Affects R12][Needs research] Układ w małym terminalu i zachowanie focusa przy przełączaniu między panelem rozmowy a panelem akcji.
- [Affects R17][Technical] Plan kolejnych przeniesień (pytania agenta, nowy task) na kontrolkę wielokrokową.
