---
date: 2026-10-22
topic: akcje-taska-z-backendu-i-komentarz-w-pauzie
---

# Akcje taska z backendu i komentarz w każdym stanie zatrzymania

## Summary

Lista akcji dostępnych na tasku (komentarz do rozmowy, opcje decyzji, odpowiedź na pytanie agenta, zatwierdzenie, zgoda na uprawnienie) pochodzi z backendu. Klient (TUI) ich nie wylicza. Task zatrzymany (pytanie, zatwierdzenie, uprawnienie, pauza operatora) zawsze oferuje komentarz do bieżącej sesji. Komentarz wraca do agenta i wznawia jego pracę. Operator nie musi wybierać akcji workflow.

---

## Problem Frame

Dziś decyzję, co operator może zrobić z taskiem, podejmuje częściowo klient. W `src/tui/task_app/actions.rs` funkcja `availability` ocenia dostępność akcji na podstawie stanu runnera. W `src/tui/task_app/waiting.rs` klient buduje panel odpowiedzi (`Model::Input`, `Model::Operator`, `Model::Owner`) i sam wybiera, czy pokazać pole komentarza (`composer_spec`). Część danych już przychodzi z serwera (`manual_transitions`, `AttentionItem`), ale reguły „co wolno teraz" są rozproszone po kliencie. Każdy nowy klient (inny niż TUI) musi je powielić. Reguły mogą się rozjechać z regułami backendu.

Drugi problem: komentarz do sesji agenta jest dziś zależny od rodzaju oczekiwania. Gdy agent czeka na zatwierdzenie lub zgodę na uprawnienie, operator wybiera z listy opcji. Gdy operator wstrzymał task, nie ma jednego, pewnego sposobu, by dopisać uwagę i wrócić do agenta. Operator chce w każdym z tych stanów po prostu powiedzieć agentowi, co myśli. Agent ma to dostać jako kontynuację tej samej sesji.

---

## Actors

- A1. Operator: osoba, która obsługuje task w TUI (lub innym kliencie) i decyduje, co dalej.
- A2. Agent: harness (np. Claude Code, Codex), który prowadzi sesję i może się zatrzymać.
- A3. Backend: silnik i protokół kontrolera. Zna stan taska i decyduje, jakie akcje są dozwolone.
- A4. Klient: TUI. Pokazuje akcje z backendu i wysyła wybór operatora.

---

## Key Flows

- F1. Odczyt dostępnych akcji
  - **Trigger:** Operator zaznacza task lub widok taska się odświeża.
  - **Actors:** A1, A3, A4
  - **Steps:** Klient pobiera od backendu opis taska razem z listą dostępnych akcji. Każda akcja ma identyfikator, etykietę, stan „dostępna / niedostępna" i powód niedostępności. Akcje z wyborem (np. opcje decyzji) niosą listę opcji. Klient pokazuje to bez własnej oceny.
  - **Outcome:** Menu i skróty pokazują dokładnie to, co backend dopuszcza.
  - **Covered by:** R1, R2, R3, R4

- F2. Komentarz do zatrzymanego taska
  - **Trigger:** Task stoi, bo agent zadał pytanie, czeka na zatwierdzenie, czeka na zgodę na uprawnienie albo operator go wstrzymał.
  - **Actors:** A1, A2, A3, A4
  - **Steps:** Operator otwiera task. Widzi akcję „Dodaj komentarz". Wpisuje tekst i wysyła. Backend przyjmuje komentarz, zapisuje go w rozmowie sesji i przekazuje agentowi. Agent wznawia pracę z tym komentarzem.
  - **Outcome:** Przetwarzanie wróciło do agenta z tekstem operatora. Operator nie wybrał żadnej akcji workflow.
  - **Covered by:** R5, R6, R7, R8, R9

---

## Requirements

**Akcje z backendu**

- R1. Backend zwraca dla każdego taska listę akcji dostępnych teraz. Dotyczy to wszystkich akcji operatora na tasku: komentarz, odpowiedź na pytanie agenta, zatwierdzenie lub odrzucenie, zgoda na uprawnienie, wstrzymanie i wznowienie, przerwanie, ręczne przejście, restart, archiwizacja.
- R2. Dla każdej akcji backend podaje: identyfikator, czy jest dostępna, powód niedostępności (tekst dla operatora) i, gdy akcja ma wybór, listę opcji.
- R3. Klient nie ocenia dostępności akcji sam. Funkcja `availability` w kliencie znika lub tylko odwzorowuje dane z backendu. Brak danych z backendu klient pokazuje jako „niedostępne: brak danych", a nie zgaduje.
- R4. Backend przyjmuje wybraną akcję i sam ją odrzuca, gdy w chwili wysłania nie jest już dozwolona. Odrzucenie zwraca powód. Klient pokazuje ten powód i odświeża listę akcji.

**Komentarz w stanie zatrzymania**

- R5. Task w stanie zatrzymania przetwarzania przez agenta zawsze ma dostępną akcję „Dodaj komentarz". Stany zatrzymania: pytanie agenta, zatwierdzenie akcji, zgoda na uprawnienie, pauza wywołana przez operatora.
- R6. Komentarz nie jest akcją workflow. Operator nie wybiera przejścia ani opcji. Komentarz wraca do agenta jako kontynuacja bieżącej sesji i przetwarzanie rusza dalej.
- R7. Komentarz jest widoczny w rozmowie taska jako wypowiedź operatora, tak samo jak odpowiedź na pytanie agenta.
- R8. Komentarz jest dostępny obok opcji decyzji, nie zamiast nich. Gdy agent pyta o zatwierdzenie, operator nadal może zatwierdzić lub odrzucić bez komentarza.
- R9. Gdy task jest zatrzymany przez operatora (pauza), wysłanie komentarza wznawia sesję agenta z tym komentarzem. Operator nie musi najpierw użyć osobnej akcji „wznów".

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Dane: backend oznacza „archiwizuj" jako niedostępne z powodem „zadanie działa". Gdy klient rysuje menu, pozycja jest wyszarzona z tym powodem. Klient nie ma własnej reguły, która by to zmieniła.
- AE2. **Covers R4.** Dane: operator wybrał akcję, a w międzyczasie task zmienił stan. Gdy wysyła wybór, backend odrzuca go z powodem. Klient pokazuje powód i odświeża listę akcji.
- AE3. **Covers R5, R6, R7.** Dane: agent zadał pytanie. Gdy operator wybiera „Dodaj komentarz" i wysyła tekst, agent dostaje go jako odpowiedź w tej samej sesji, a rozmowa pokazuje wypowiedź operatora.
- AE4. **Covers R5, R8.** Dane: agent czeka na zgodę na uprawnienie. Lista akcji zawiera „Zezwól", „Odmów" i „Dodaj komentarz". Gdy operator wybiera „Zezwól", nic więcej nie jest wymagane.
- AE5. **Covers R5, R9.** Dane: operator wstrzymał task. Gdy dodaje komentarz, sesja agenta rusza dalej z tym komentarzem, a stan taska przestaje być „wstrzymany".
- AE6. **Covers R3.** Dane: opis taska nie zawiera listy akcji (stary lub niedostępny backend). Klient pokazuje akcje jako niedostępne z powodem „brak danych" i nie wylicza ich ze stanu.

---

## Success Criteria

- Operator w każdym zatrzymaniu agenta widzi akcję „Dodaj komentarz" i może wrócić do agenta własnym tekstem.
- Nowy klient (poza TUI) może obsłużyć task, używając tylko danych z backendu, bez kopiowania reguł dostępności.
- Planista nie musi wymyślać, które stany zatrzymania dają komentarz, ani kto jest źródłem prawdy o dostępności akcji. Opisują to R1–R9.

---

## Scope Boundaries

- Brak zmiany treści i znaczenia akcji workflow (przejścia, `allow_manual_override`). Zmienia się tylko źródło informacji o dostępności.
- Brak komentarza do taska, który nie ma sesji agenta (np. zakończony, zarchiwizowany, w kolejce bez sesji). Komentarz dotyczy stanów zatrzymania z R5.
- Brak zmiany wyglądu i skrótów TUI poza tym, co wynika z nowej akcji „Dodaj komentarz" i z braku lokalnej oceny dostępności.
- Brak nowych uprawnień i modelu ról. Kto może wykonać akcję, określa dotychczasowa autoryzacja.
- Brak komentarzy wielu autorów, wątków i edycji komentarza po wysłaniu.

---

## Key Decisions

- Backend jest jedynym źródłem prawdy o dostępności akcji. Dlaczego: jedna reguła dla wszystkich klientów i brak rozjazdu z walidacją po stronie serwera (R4 i tak ją egzekwuje).
- Komentarz nie jest przejściem workflow. Dlaczego: operator w zatrzymaniu chce rozmawiać z agentem, nie sterować grafem stanów. Workflow dalej prowadzi agent.
- Komentarz w pauzie wznawia agenta. Dlaczego: inaczej operator musiałby użyć dwóch akcji (wznów, potem napisz). Ta decyzja jest założeniem do potwierdzenia (patrz niżej).

---

## Dependencies / Assumptions

- Zweryfikowane w kodzie: klient dziś sam ocenia dostępność (`availability` w `src/tui/task_app/actions.rs`) i sam buduje panel odpowiedzi (`src/tui/task_app/waiting.rs`). Backend zwraca już część danych: `manual_transitions`, `AttentionItem` (`AttentionKind`: pytanie, zatwierdzenie, problem, limit wejścia, nieudany krok, propozycja conductora, nieudane provisioning) w `crates/narsor-protocol/src/controller.rs`.
- Niezweryfikowane: czy backend ma dziś jedną ścieżkę dostarczenia komentarza do sesji agenta we wszystkich stanach z R5, w tym w pauzie. Istnieją osobne punkty wejścia (`control_input`, `resolve_permission`, `control_operator_command`, komendy właściciela). Do sprawdzenia w planowaniu.

---

## Założenia do potwierdzenia

- Z1. „Stan zatrzymania" obejmuje cztery przypadki z opisu: pytanie agenta, zatwierdzenie akcji, zgoda na uprawnienie, pauza operatora. Nie obejmuje nieudanego kroku, problemu runnera ani nieudanego provisioning. W tych stanach komentarz nie jest wymagany.
- Z2. Komentarz do wstrzymanego taska wznawia agenta jednym krokiem (R9). Alternatywa: komentarz zostaje zapisany i czeka na osobne „wznów".
- Z3. Komentarz jest zwykłym tekstem. Załączniki i materiały są poza zakresem.
- Z4. Lista akcji z backendu jest częścią opisu taska, który klient i tak pobiera. Nie wymaga osobnego zapytania na każdy task. (Szczegół do decyzji w planowaniu, ale zakłada, że koszt odczytu listy nie rośnie wyraźnie.)
- Z5. Klient zachowuje własne, czysto prezentacyjne wyszarzanie i skróty. Źródłem prawdy o tym, co wolno, jest backend.

## Pytania otwarte dla użytkownika

- P1. Czy komentarz w pauzie ma wznawiać agenta od razu (Z2), czy operator ma mieć wybór, np. „zapisz komentarz" i „wyślij i wznów"?
- P2. Czy pytanie agenta z predefiniowanymi opcjami ma pokazywać komentarz jako osobną opcję, czy jako zawsze obecne pole obok opcji?
- P3. Czy komentarz dla „zgody na uprawnienie" ma znaczyć „odmów z uwagą" (agent dostaje odmowę i tekst), czy „bez decyzji, tylko uwaga" (zgoda nadal czeka)? To zmienia, czy task po komentarzu nadal stoi.
- P4. Czy lista akcji z backendu ma obejmować także akcje, które dziś klient zna lokalnie (przenoszenie w kolejce, otwarcie linku), czy tylko akcje związane z rozmową i decyzjami (zakres R1)?

## Outstanding Questions

### Resolve Before Planning

- [Affects R6, R9][User decision] Patrz P1 i P3: zachowanie komentarza w pauzie i przy zgodzie na uprawnienie.

### Deferred to Planning

- [Affects R1, R2][Technical] Jak opisać akcje w protokole kontrolera i jak zachować zgodność ze starszymi klientami.
- [Affects R6, R9][Needs research] Jak backend dostarcza komentarz do sesji agenta w każdym stanie z R5 i czy harnessy (Claude Code, Codex, opencode) to obsługują tak samo.
- [Affects R4][Technical] Jak backend ocenia aktualność wyboru (generacja taska, klucz idempotencji) przy odrzuceniu.
