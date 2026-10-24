---
title: "feat: Akcja 0 „Napisz do agenta” w każdym oczekiwaniu i w pauzie"
type: feat
status: active
date: 2026-10-24
origin: docs/brainstorms/2026-10-24-czat-z-agentem-w-kazdym-oczekiwaniu-requirements.md
---

# feat: Akcja 0 „Napisz do agenta” w każdym oczekiwaniu i w pauzie

## Summary

Backend już ma jeden punkt wejścia dla komentarza do agenta (`submit_comment`, akcja `TaskActionId::Comment` na liście akcji z `TaskSummary.actions`). Klient ma okno komentarza (`c`, `Overlay::Comment`). Plan nie buduje nowej ścieżki. Plan robi trzy rzeczy:

1. Ustawia „Napisz do agenta” jako stałą pozycję 0 w panelu oczekiwania (wszystkie trzy modele: `Input`, `Operator`, `Owner`) i w menu akcji (`.`).
2. Pokazuje akcję jako niedostępną z powodem, gdy backend jej nie oferuje (R3).
3. Zachowuje wpisany tekst, gdy wysłanie się nie uda (R8).

Dostępność zawsze pochodzi z listy akcji backendu (R10, decyzja z dokumentu `2026-10-22-akcje-taska-z-backendu...` jest już zrealizowana w kodzie: zob. `docs/plans/2026-10-22-003-feat-akcje-taska-z-backendu-i-komentarz-w-pauzie-plan.md`). Dostępność w backendzie rozszerza się tylko tam, gdzie istnieje sesja agenta, której można dostarczyć wiadomość (U1).

---

## Problem Frame

Dziś pole wiadomości w panelu oczekiwania istnieje tylko dla `InputKind::AgentQuestion` i `InputKind::Problem` (`composer_spec` w `src/tui/input_form.rs`). Panel zatwierdzenia, `OperatorPanel` i `OwnerPanel` w `src/tui/task_app/waiting.rs` nie mają go. Komentarz istnieje, ale tylko jako ukryty skrót `c` i pozycja na końcu menu (`TaskAction::ALL` w `src/tui/task_app/actions.rs`). Operator w panelu nie widzi go. Backend odmawia komentarza w `comment_reason` (`crates/narsor-engine/src/controller_protocol/service/actions.rs`) dla ciągów `CompleteStep`, `EntryLimit`, nieudanego kroku, problemu runnera i nieudanego provisioningu (see origin: Problem Frame, R1).

---

## Requirements

Wymagania R1–R10, przepływy F1–F2 i AE1–AE6 pochodzą z dokumentu źródłowego. Plan nie zmienia ich znaczenia.

- R1. Akcja dostępna w każdym oczekiwaniu i w pauzie, gdy istnieje agent do adresowania. → U1, U2, U3
- R2. Brak zależności od workflow; `WORKFLOW_SCHEMA.md` bez zmian. → U2 (akcja jest dodawana przez klienta i backend, nie przez definicję workflow)
- R3. Bez sesji agenta akcja widoczna jako niedostępna z powodem. → U1, U2
- R4. Stała pozycja 0; numeracja pozostałych może się przesunąć (Z6). → U2, U3
- R5. Jedno pole; pusta treść nie jest wysyłana, z powodem. → U2, U4
- R6. Skutek zależny od stanu (pauza: wznowienie; zatwierdzenie i uprawnienie: odmowa z uwagą; pytanie: odpowiedź). → istniejący `comment_delivery`; U1 go tylko potwierdza testami
- R7. Wiadomość widoczna w rozmowie, pole puste po wysłaniu. → U2, U4
- R8. Błąd wysłania zachowuje tekst. → U4
- R9, Z3. Istniejące pola odpowiedzi działają jak dziś; akcja 0 otwiera to samo pole, nie drugie. → U2
- R10. Dostępność ocenia backend. → U1, U2

**Origin actors:** A1–A4
**Origin flows:** F1 (U2), F2 (U3)
**Origin acceptance examples:** AE1 (U2), AE2 (U2), AE3 (U1, U2), AE4 (U2, U4), AE5 (U4), AE6 (U1, U3)

---

## Scope Boundaries

Bez zmian względem dokumentu źródłowego (see origin: Scope Boundaries): brak rozmowy z dyrygentem, brak załączników i wątków, brak wysyłania do taska bez sesji agenta, brak nowych ról, brak przełączania Esc, brak zwykłej rozmowy poza oczekiwaniem.

### Deferred to Follow-Up Work

- Zwykła rozmowa z agentem niezależna od oczekiwania. Plan tylko wydziela okno pola tak, by dało się je użyć poza panelem (Z7, KTD3). Nie dodaje go nigdzie indziej.
- Polecenie CLI `task comment` (już odłożone w poprzednim planie).

---

## Context & Research

### Stan kodu

- Backend: `crates/narsor-engine/src/controller_protocol/service/actions.rs` (`OFFERED` zaczyna się od `Comment`, `comment_reason`, `comment_delivery`, `submit_comment`), `service.rs` (blokada drugiego komentarza w pauzie, ok. linii 2092).
- Protokół: `crates/narsor-protocol/src/controller.rs` (`TaskActionId`, `TaskActionOffer` z `effect` i `reason`), `SubmitCommentRequest`.
- Klient, lista: `src/tui/task_app/actions.rs` (`TaskAction::Comment`, `availability`, `menu_items`, `comment_form`, `chosen_comment`, `Overlay::Comment`, `ActionSend::Comment`).
- Klient, panel: `src/tui/task_app/waiting.rs` (`Model`, `Model::items`, `composer_item`, `Entry::build`, `Entry::rebuild`, `WaitingPanel::prepare`, `submit_answer`, `settle`) i `src/tui/input_form.rs` (`composer_spec`).
- Skróty: `src/tui/keymap.rs` (`Action::Comment`, konteksty `Menu` i `Panel`).
- Testy: `tests/controller_protocol.rs`, `tests/tui_state.rs`, `tests/tui_render.rs`, `tests/task_app_support/mod.rs`.

### Wnioski z `docs/solutions/`

- `design-patterns/tui-focus-stays-in-details-after-confirmed-action-2026-10-20.md`: po wysłaniu fokus zostaje w szczegółach.
- `design-patterns/tui-reads-as-keyed-background-jobs-with-loading-marker-2026-10-22.md`: wysyłka idzie jako zadanie w tle z kluczem; klucz idempotentności wraca przy ponowieniu.
- `design-patterns/tui-modal-confirmation-keyboard-contract.md`: akcja 0 nie dodaje okna `y`/`n` (jeden krok, R5).

### Odpowiedzi na pytania odłożone do planowania

- Lista akcji z backendu istnieje (R10 spełnione bez oceny po stronie klienta). Akcja 0 nie wymaga tymczasowej logiki klienta.
- Backend dostarcza wiadomość w czterech stanach (pytanie, zatwierdzenie, uprawnienie, pauza). Nie dostarcza jej dla `CompleteStep`, `EntryLimit`, nieudanego kroku, problemu runnera i nieudanego provisioningu. To jest luka względem R1 (U1).
- Czy każdy harness (Claude Code, Codex, opencode) wznawia sesję z wiadomością w pauzie: **nie zweryfikowane w planie**. Wznowienie z wiadomością używa wzorca z `docs/plans/2026-10-05-0003-feat-kontynuacja-rozmowy-agenta-plan.md`. Zob. Deferred to Implementation.

---

## Key Technical Decisions

- **KTD1. Bez nowej ścieżki wysyłki.** Akcja 0 używa `TaskActionId::Comment` i `submit_comment`. Dlaczego: jedna reguła „co wolno" i jedna ścieżka dostarczenia (R10, Z5). Odrzucono osobną akcję backendu „message".
- **KTD2. Akcja 0 jest wpisem listy, nie polem na stałe.** W modelach `Operator` i `Owner` i w `Input` bez własnego pola (zatwierdzenie, uprawnienie) wpis 0 ma pole wielolinijkowe (`ChooserField::multiline(...).required()`), jak `comment_form`. Wpis jest generowany w jednym miejscu (`Model::items`), więc nowy rodzaj oczekiwania dostaje akcję bez dodatkowej pracy (kryterium sukcesu 2).
- **KTD3. Pole jako samodzielny element.** Budowa wpisu (`write_to_agent_item(task)`) jest funkcją wolną w `src/tui/task_app/actions.rs`, bez zależności od panelu. `comment_form` i panel używają jej obydwa. Dlaczego: Z7, przyszła zwykła rozmowa.
- **KTD4. Z3: dla pytania agenta i problemu wpis 0 to istniejące pole.** `Model::items` już ukrywa wpis z polem (`composer_index`) i pokazuje go jako osobny `composer`. Wpis 0 w takim panelu **ustawia fokus na tym polu** i nie tworzy drugiego. Wysyłka idzie starą ścieżką (`Plan::Answer` / `Plan::Message`), bo daje to samo co `comment_delivery` (`ResumeAgent` → odpowiedź).
- **KTD5. Pauza nie ma panelu oczekiwania.** Task w stanie `Stopped`/`Paused` nie ma `AttentionItem`. Powierzchnią akcji 0 jest tam menu akcji (`.`) i skrót `c`. Pozycja 0 w menu = „Napisz do agenta" (`TaskAction::ALL` zaczyna się od `Comment`). Założenie do potwierdzenia: R4 mówi o „panelu oczekiwania", a F2 o „otwiera task", więc pauza korzysta z menu. Zob. Open Questions.
- **KTD6. Rozszerzenie backendu ograniczone do realnej sesji.** `comment_reason` pozwala na komentarz tylko, gdy `comment_delivery` ma dokąd go dostarczyć. Wpis jest niedostępny z powodem w pozostałych przypadkach (R3). Dlaczego: R1 mówi „o ile istnieje agent, któremu można wysłać komunikat".

---

## Implementation Units

### U1. Backend: macierz dostępności komentarza i testy

**Goal:** Ustalić i zabezpieczyć testami, w których oczekiwaniach akcja `Comment` jest dostępna, a w których niedostępna z powodem. Rozszerzyć ją tam, gdzie sesja agenta istnieje.

**Requirements:** R1, R3, R6, R10; AE3, AE6

**Dependencies:** brak

**Files:**
- Modify: `crates/narsor-engine/src/controller_protocol/service/actions.rs` (`comment_reason`, `comment_effect`, `comment_delivery`, tylko jeśli macierz wykaże lukę)
- Test: `tests/controller_protocol.rs`

**Approach:**
- Zbuduj macierz: rodzaj oczekiwania (`ResumeAgent`, `ResolveOutput`, `ApproveOutput`, `ResolvePermission`, `CompleteStep`, `EntryLimit`, nieudany krok, problem runnera, nieudany provisioning, pauza, kolejka, zakończony) × dostępność i powód.
- Dla każdego niedostępnego wiersza sprawdź w kodzie silnika (`crates/narsor-engine/src/engine/actions/human.rs`), czy sesja agenta żyje i czy `agent_message` da się dostarczyć. Jeśli tak, dodaj gałąź do `comment_delivery` i zdejmij odmowę. Jeśli nie, zostaw odmowę i dopracuj powód (słowa po polsku, jedna linia, ASD-STE100).
- „Awaria providera" z R1: ustal, jakim rodzajem oczekiwania jest (prawdopodobnie `Problem` → `ResumeAgent`, już dostępne). Zapisz wynik w teście.

**Execution note:** Zacznij od testu charakteryzującego obecną macierz, potem zmień kod.

**Patterns to follow:** istniejące testy komentarza w `tests/controller_protocol.rs`.

**Test scenarios:**
- Happy path: dla każdego rodzaju oczekiwania z dostarczeniem `actions[Comment].available == true` i `effect` ma właściwe słowa (pytanie, zatwierdzenie, uprawnienie, pauza).
- Covers AE3. Zakończony task i task w kolejce: `available == false`, powód mówi o braku sesji agenta.
- Error: nieudany provisioning i problem runnera bez sesji: `available == false` z powodem.
- Covers AE6. Task w pauzie: `submit_comment` zapisuje wiadomość, wznawia task, stan przestaje być wstrzymany.
- Edge: drugi komentarz w pauzie przed odebraniem pierwszego jest odmówiony, a oferta tego nie obiecuje (istniejące zachowanie, test regresji).
- Integration: odmowa uprawnienia z komentarzem nie udziela zgody (`PermissionDecisionKindDto::Deny`).

**Verification:** macierz w teście opisuje każdy wiersz; żaden wiersz nie jest dostępny bez miejsca dostarczenia.

---

### U2. Klient: wpis „Napisz do agenta” na pozycji 0 w panelu oczekiwania

**Goal:** Każdy model panelu (`Input`, `Operator`, `Owner`) zaczyna listę od wpisu „Napisz do agenta”, dostępnego albo niedostępnego z powodem z backendu.

**Requirements:** R1, R2, R3, R4, R5, R7, R9, R10; F1; AE1, AE2, AE3, AE4

**Dependencies:** U1

**Files:**
- Modify: `src/tui/task_app/waiting.rs` (`Model::items`, `Model::selected`, `Entry::build`, `Entry::rebuild`, `WaitingPanel::prepare`, `on_key`)
- Modify: `src/tui/task_app/actions.rs` (wydzielona funkcja budująca wpis: KTD3; wspólna z `comment_form`)
- Modify: `src/tui/input_form.rs` (tylko jeśli `composer_spec` wymaga wskazania wpisu, który fokusuje wpis 0)
- Test: `tests/tui_state.rs`, `tests/tui_render.rs`, `tests/task_app_support/mod.rs`

**Approach:**
- `Model::items` dostaje podsumowanie zadania (lub gotowy `Availability`) i wstawia wpis 0 przed pozostałe. Indeksy `Submission.index` i `selected()` przesuwają się o 1; `prepare` i `delivery` muszą traktować indeks 0 jako wpis „Napisz do agenta”, a resztę mapować z `index - 1`. To jedno miejsce konwersji, nie rozsiane `+ 1`.
- Dostępność: `availability(task, TaskAction::Comment)` (backend). Gdy niedostępna: `ChooserItem::unavailable(reason)`; wybór pokazuje powód, nie otwiera pola.
- Wysyłka wpisu 0: `Request::Act` z `ActionSend::Comment`, jak `chosen_comment`. Skutek mówi `effect` z oferty.
- Z3/KTD4: dla `Input` z `composer_spec()` wpis 0 przenosi fokus do istniejącego `composer` i nie wysyła nic sam.
- Domyślne zaznaczenie nadal pochodzi z rekomendacji dyrygenta lub najbezpieczniejszej opcji; wpis 0 nie jest domyślny (`selecting(model.selected())` liczy się po przesunięciu). `rebuild` nie może przesunąć zaznaczenia operatora, który nie ruszył listy (`untouched`).
- Numeracja skrótów (Z6): sprawdź, że żaden test ani plik pomocy nie zakłada „1 = pierwsza akcja workflow”.

**Patterns to follow:** `Model::items` i `Entry::build` w `waiting.rs`; `ChooserItem::unavailable`; `comment_form`.

**Test scenarios:**
- Covers AE1. Gate z zatwierdzeniem bez pola wiadomości: panel ma wpis 0 „Napisz do agenta”, dostępny.
- Covers AE2. Pytanie agenta: wybór wpisu 0 ustawia fokus na polu odpowiedzi (jedno pole, nie dwa); wpisanie „użyj innej biblioteki” i Enter wysyła odpowiedź starą ścieżką; rozmowa pokazuje wypowiedź.
- Covers AE3. Task bez aktywnej sesji: wpis 0 widoczny jako niedostępny z powodem „brak aktywnej sesji agenta” lub słowami backendu.
- Covers AE4. Wpis 0, puste pole, zatwierdzenie: nic nie jest wysłane, na pasku powód.
- Edge: `OperatorPanel` (nieudany krok) i `OwnerPanel` (provisioning, problem runnera) mają wpis 0; stan dostępności zgodny z ofertą backendu.
- Edge: zmiana oferty w trakcie (odświeżenie): `rebuild` zachowuje wpisany tekst i zaznaczenie.
- Edge: brak oferty (stary backend, `actions == None`): wpis 0 niedostępny z `NO_OFFER`, panel nie panikuje.
- Integration: indeks wysłanej opcji workflow nie przesuwa się (wybór „Zatwierdź” po wstawieniu wpisu 0 wysyła właśnie zatwierdzenie).
- Render: wpis 0 jest pierwszy; reszta zachowuje kolejność.

**Verification:** w każdym z trzech modeli i w `Missing` zachowanie wpisu 0 jest opisane testem; żaden istniejący test opcji workflow nie zmienił wyniku poza numerami.

---

### U3. Klient: pozycja 0 w menu akcji i pauza

**Goal:** Menu akcji (`.`) zaczyna się od „Napisz do agenta”, więc task w pauzie (bez panelu oczekiwania) ma akcję 0. Skrót `c` działa jak dotąd.

**Requirements:** R1, R4; F2; AE6

**Dependencies:** U1

**Files:**
- Modify: `src/tui/task_app/actions.rs` (`TaskAction::ALL`, nazwa w `menu_text`, `verb`)
- Modify: `src/tui/keymap.rs` (tekst pomocy i opis `Action::Comment`)
- Test: `tests/tui_state.rs`, `tests/tui_render.rs`

**Approach:**
- `TaskAction::ALL` zaczyna się od `Comment`; rozmiar tablicy zostaje (8). Nazwa w menu: „napisz do agenta” (zamiast „dodaj komentarz”). Opis (`effect`) z oferty backendu.
- Okno pola (`comment_form`) używa funkcji z U2 (KTD3). Etykieta okna: „napisz do agenta”.
- Zmień komunikaty paska (`verb`: „komentarz”) tylko, jeśli słowo „komentarz” myli; w przeciwnym razie zostaw dla zgodności testów.

**Patterns to follow:** `menu_items`, `menu_text`.

**Test scenarios:**
- Covers AE6. Task w pauzie: menu ma wpis 0 dostępny; wysłanie wznawia task i stan przestaje być „wstrzymany”.
- Edge: task w kolejce: wpis 0 niedostępny z powodem; pozostałe wpisy bez zmian.
- Edge: brak połączenia (`mutation_allowed` odmawia): wpis 0 zablokowany jak inne zmieniające stan.
- Render: kolejność menu: Napisz do agenta, Przesuń, Pauza/Wznów, …

**Verification:** `TaskAction::ALL` i menu zgodne; skrót `c` otwiera to samo okno co wpis 0.

---

### U4. Klient: błąd wysyłki zachowuje tekst

**Goal:** Gdy wysłanie nie udaje się (sesja zakończona, odrzucenie przez backend, brak połączenia), operator widzi powód i nie traci tekstu (R8).

**Requirements:** R5, R7, R8; AE4, AE5

**Dependencies:** U2, U3

**Files:**
- Modify: `src/tui/task_app/actions.rs` (`chosen_comment`, `perform` dla `ActionSend::Comment`, obsługa błędu)
- Modify: `src/tui/task_app/waiting.rs` (`settle`, `answer`: wpis 0 w panelu)
- Test: `tests/tui_state.rs`

**Approach:**
- Panel: wysyłka wpisu 0 niesie klucz (`Entry.key`); błąd zapisuje `entry.error` i zostawia pole z tekstem (jak istniejące pola). Sukces czyści pole i ustawia `sent` (R7).
- Okno z menu: `chosen_comment` bierze `form` przez wartość, więc `overlay` zamyka się przed wysyłką i tekst ginie. Zachowaj tekst: przy błędzie otwórz okno ponownie z poprzednią treścią i powodem (lub trzymaj szkic w `TaskApp`, jak `detail.set_draft` dla panelu).
- Powód: słowa backendu (`failure_reason`); nie nowe teksty po stronie klienta.
- Ponowienie tej samej decyzji używa tego samego klucza idempotentności (`action_keys`), więc podwójne wysłanie nie duplikuje wiadomości.

**Execution note:** Zacznij od testu, który wysyła komentarz do mocka odmawiającego i sprawdza, że tekst wraca do pola.

**Patterns to follow:** `settle` i `settle_answer` w `waiting.rs`; `action_keys` w `actions.rs`.

**Test scenarios:**
- Covers AE5. Sesja zakończyła się w chwili wysyłania: backend odmawia; TUI pokazuje powód; pole nadal ma tekst, także po zamknięciu i otwarciu oczekiwania.
- Error: `stale_generation`: powód widoczny, tekst zachowany, odświeżenie listy.
- Error: błąd transportu: powód, tekst zachowany, ponowienie używa tego samego klucza.
- Happy path: sukces czyści pole i pokazuje, że wysłano (R7).
- Edge: tekst wielolinijkowy (Ctrl+J) wraca bez zmian.

**Verification:** po każdym błędzie tekst wpisany przez operatora jest w polu bez ponownego pisania.

---

### U5. Dokumentacja i pomoc w TUI

**Goal:** Opis nowej pozycji 0 w pomocy TUI i w dokumentacji użytkownika; potwierdzenie, że schemat workflow się nie zmienia.

**Requirements:** R2, R4

**Dependencies:** U2, U3

**Files:**
- Modify: `src/tui/keymap.rs` (pomoc)
- Modify: `README.md` (jeśli opisuje akcje taska i skróty)
- Test: `Test expectation: none -- tylko tekst pomocy i dokumentacja; pokrycie w testach renderu z U2 i U3`

**Approach:** Opisz akcję 0 jednym zdaniem w stylu ASD-STE100: krótkie zdania, jedno znaczenie słowa „komentarz” / „wiadomość”. Nie dodawaj pola do `WORKFLOW_SCHEMA.md` (R2).

**Verification:** `WORKFLOW_SCHEMA.md` bez zmian; pomoc i README zgodne z menu.

---

## System-Wide Impact

- **Interaction graph:** `Submission.index` w panelu przesuwa się o 1 dla wszystkich opcji workflow (U2). To najbardziej ryzykowna zmiana: pomyłka wyśle złą opcję (np. zatwierdzenie zamiast odrzucenia).
- **Error propagation:** błąd wysyłki musi dojść do pola, nie tylko do paska (U4).
- **Stare backendy:** `actions == None` daje wpis niedostępny, nie awarię (U2).
- **Unchanged invariants:** `submit_comment` jako jedyny punkt wejścia; `comment_delivery` nie udziela zgody; Esc bez zmian; czat dyrygenta bez zmian.

---

## Risks & Dependencies

| Ryzyko | Skutek | Środek |
|---|---|---|
| Przesunięcie indeksów opcji w panelu | Wysłana zła opcja workflow | Jedno miejsce konwersji indeksu (U2) i test na wysłaną opcję |
| Rozszerzenie `comment_reason` wysyła wiadomość do sesji, która jej nie przyjmie | Cichy brak dostarczenia | U1 rozszerza tylko tam, gdzie silnik ma ścieżkę dostarczenia; reszta niedostępna z powodem |
| Harness nie wznawia sesji z wiadomością w pauzie | Wiadomość nie dociera | Zob. Deferred to Implementation |
| Okno z menu gubi tekst przy błędzie | R8 niespełnione | U4 |

---

## Open Questions

### Deferred to Implementation

- Czy Claude Code, Codex i opencode wznawiają sesję z wiadomością z pauzy tak samo. Sprawdza to U1 przy testach integracyjnych silnika; jeśli któryś harness nie wspiera, wpis 0 dla tego harnessu jest niedostępny z powodem (R3), a luka wraca do autora wymagań.
- Które z odmów `comment_reason` (nieudany krok, `CompleteStep`, `EntryLimit`) mają żywą sesję agenta. Rozstrzyga U1 z kodu silnika.
- Dokładne słowa powodów po polsku.

### Assumption to Confirm

- KTD5: dla taska w pauzie akcja 0 jest w menu akcji (`.`, `c`), bo pauza nie ma panelu oczekiwania. Jeśli autor wymagań chce akcji 0 także w panelu dla pauzy, wymaga to nowego panelu pauzy i wraca do wymagań.

---

## Sources & References

- **Origin document:** `docs/brainstorms/2026-10-24-czat-z-agentem-w-kazdym-oczekiwaniu-requirements.md`
- Poprzedni plan: `docs/plans/2026-10-22-003-feat-akcje-taska-z-backendu-i-komentarz-w-pauzie-plan.md`
- Wzorzec wznowienia sesji: `docs/plans/2026-10-05-0003-feat-kontynuacja-rozmowy-agenta-plan.md`
