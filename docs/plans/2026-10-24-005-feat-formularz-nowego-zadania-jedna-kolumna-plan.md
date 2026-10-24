---
title: "feat: Formularz nowego zadania w jednej kolumnie, tło całego pola, limity Ticket/Tytuł"
type: feat
status: active
date: 2026-10-24
origin: docs/brainstorms/2026-10-24-formularz-nowego-zadania-pola-pod-soba-requirements.md
---

# feat: Formularz nowego zadania w jednej kolumnie, tło całego pola, limity Ticket/Tytuł

## Summary

Trzy zmiany w TUI. (1) `src/tui/chooser/inputs.rs` rysuje wszystkie pola jedno pod drugim zamiast paska krótkich pól. Kolejność nawigacji jest kolejnością deklaracji pól. (2) Pole wieloliniowe ma tło w całym prostokącie, także w pustych wierszach. (3) Pola Ticket i Tytuł mają limit 15 i 50 znaków, tylko w formularzu TUI, bez licznika znaków. Backend nie zmienia się (see origin: `docs/brainstorms/2026-10-24-formularz-nowego-zadania-pola-pod-soba-requirements.md`).

---

## Problem Frame

Dziś `inputs::render` rysuje pasek (`render_bar`), linię statusu i pola wieloliniowe (`render_long_fields`). `focus_order_of` w `src/tui/chooser.rs` daje najpierw pola krótkie, potem długie. Pasek układa pola poziomo, a `↑`/`↓` idą w pionie.

Poprawka szerokości tła (#24, `docs/plans/2026-10-24-001-fix-tui-tlo-aktywnego-pola-plan.md`) jest już w kodzie: `fill_padding` i `value_lines` w `src/tui/form/field.rs`. Tło pola pełnego sięga do `text_width`. Brakuje wysokości: `main_body` w `inputs.rs` zwraca tylko wiersze tekstu, a potem `rows.resize(main_height, Line::raw(""))` dodaje wiersze bez tła. To jest przyczyna tła tylko pod tekstem (R7). Ten plan scala szerokość i wysokość w jednej regule wspólnego rysowania.

Pola `Ticket` i `Tytuł` powstają w `label_fields()` w `src/tui/task_app/new_task.rs` i są dołączane po wejściach workflow (`workflow_item`). Pole `ChooserField` nie ma dziś limitu długości. `pasted_text` w `src/tui/input_form.rs` zamienia nowe linie na spacje dla pola jednolinijkowego, ale nie obcina.

Limit tytułu 50 znaków to dziś `GENERATED_TITLE_MAX_CHARS` w `crates/narsor-engine/src/domain/task.rs`. Główny crate (`Cargo.toml`) nie zależy od `narsor-engine`, więc formularz nie może go zaimportować.

---

## Requirements Trace

| Wymaganie | Jednostka |
|---|---|
| R1, R2, R3, R6 | U1, U2 |
| R4, R5 | U1, U2 |
| R7, R8, R9, R10, R11 | U1 |
| R12–R15 | U3 |
| R16 (licznik) | Poza zakresem: operator nie chce licznika znaków |
| R17 | U3 (brak zmiany backendu), U4 (test) |
| R18 | U3 |
| AE1, AE9, AE10 | U1, U2 |
| AE2, AE3, AE4 | U1 |
| AE5, AE6, AE7 | U3 |
| AE8 | U4 |

---

## Scope Boundaries

- Bez zmiany koloru `field_editing`, motywów i trybu bez koloru.
- Bez zmiany wyboru workflow, paska z nazwą workflow i opisu (`render_strip`).
- Bez zmiany limitów serwera: 64 znaki (ticket) i 200 znaków (tytuł) w `crates/narsor-engine/src/domain/task.rs`. Bez zmiany `src/cli/remote.rs`.
- Bez limitu w polu własnym workflow o nazwie `ticket_id` albo podobnej. Limit dotyczy tylko pól `Ticket` i `Tytuł` formularza.
- Bez zmian w innych widokach (nagłówek szczegółów, zakładka Info) i w późniejszej edycji ticketu i tytułu.
- Bez nowych pól w formularzu.
- Bez licznika znaków (`7/15`) w wierszu pola. Operator widzi limit tylko przez to, że dodatkowy znak nie jest przyjęty.

### Deferred to Follow-Up Work

- Szerokość komórki znaku (`cell_width` zwraca dziś 1). Limit liczy znaki (`chars()`), jak chce R15.

---

## Key Technical Decisions

- **Jedna kolumna dla każdego `inputs_layout`.** `inputs::render` jest używany tylko przez `render_with_style` gdy `inputs_layout` i focus jest w polach. Pasek krótkich pól (`render_bar`) znika całkowicie (odpowiedź na pytanie z origin: pasek zostaje tylko jeśli ma użytkownika, a nie ma: formularz odpowiedzi i komentarza dostaje ten sam układ pionowy, zgodnie z R11). Funkcje `render_bar`, `water_level`, `window_around_cursor`, `bar_text` albo znikają, albo zostają tylko te, które wiersz krótkiego pola nadal potrzebuje.
- **Kolejność nawigacji to kolejność deklaracji.** `focus_order_of` przy `inputs_layout` zwraca `0..len`. Kolejność Ticket, Tytuł, Repo, inne wejścia, Opis ustawia `workflow_item` w `new_task.rs` przez kolejność dołączania pól. Dzięki temu nawigacja i rysowanie mają jedno źródło (R3).
- **Wiersz pola krótkiego:** jeden wiersz `nazwa: wartość`, tło `field_editing` do końca wiersza (do `text_width`, jak `fill_padding`), okno wokół kursora dla długiej wartości. Opis i `(wymagane)` aktywnego pola zostają w linii statusu (`render_status`). Pole wyboru pokazuje `‹ wartość ›`.
- **Pole Opis** to pole wieloliniowe, które `Chooser::main_field` wskazuje (pierwsze wymagane, inaczej pierwsze długie). Dostaje całą pozostałą wysokość. Inne pola wieloliniowe zwijają się jak dziś (`folded_rows`).
- **Tło całego prostokąta:** `main_body` dopełnia wiersze do `main_height` wierszami z tłem (`fill_padding` z `used = 0`, z prefiksem `CONTINUATION`), nie pustymi `Line::raw("")`. Dla pola nieaktywnego dopełnienie zostaje puste, bez tła (R10). Pole zwinięte nie ma tła, bo nie ma focusu.
- **Niski terminal (pytanie z origin).** Kolumna nie przycina Opisu do zera. Gdy wysokość nie wystarcza na wszystkie wiersze i `MIN_MAIN_ROWS` wierszy Opisu, kolumna przewija się tak, by wiersz aktywnego pola był widoczny (jak `render_fields` z `focus_line`). Wiersze krótkich pól nie giną po cichu.
- **Limit długości w polu, nie w formularzu.** `ChooserField` dostaje opcjonalny limit znaków i budowniczy `limited(n)`. `edit` i `paste` pilnują limitu. Dzięki temu silnik jednego pola daje R14 i R15 dla każdego formularza, a tylko `label_fields()` ustawia 15 i 50.
- **R18: jedna stała.** Przenieść `GENERATED_TITLE_MAX_CHARS` do `crates/narsor-protocol` (np. obok `display`), a `crates/narsor-engine/src/domain/task.rs` ma ją reeksportować. Formularz importuje ją z `narsor-protocol`. Limit ticketu (15) to nowa stała w `new_task.rs`: nie jest współdzielony z dyrygentem. Przed przeniesieniem sprawdzić, czy `narsor-engine` zależy od `narsor-protocol` (powinien).
- **Wklejanie.** `pasted_text` zostaje bez zmian (nowa linia to spacja, Z8). `ChooserField::paste` obcina wynik do wolnego miejsca (`limit - znaki w polu`), po wstawieniu w miejscu kursora.

---

## Implementation Units

### U1. Pionowa kolumna i pełne tło pola w `inputs.rs`

**Goal:** `inputs::render` rysuje strip, linię statusu i potem kolumnę pól w kolejności fokusu. Pola wieloliniowe mają tło w całym prostokącie.

**Requirements:** R1, R2, R3, R4, R5, R6, R7, R8, R9, R10, R11; AE1–AE4, AE9, AE10.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/chooser/inputs.rs`
- Modify: `src/tui/chooser.rs` (`focus_order_of`, `main_field`)
- Test: `tests/tui_render.rs`, `tests/tui_state.rs`

**Approach:**
- `focus_order_of`: przy `inputs_layout` zwraca kolejność deklaracji. Zaktualizować komentarz do `focus_order` i `with_inputs_layout`.
- `render`: po `render_strip` i `render_status` jedna funkcja rysuje kolumnę po `focus_order`. Pole krótkie to jeden wiersz. Pole długie to nagłówek (`header_line`), błąd, treść. Pole `main` bierze pozostałą wysokość po innych polach.
- Pole krótkie aktywne: `Focus` na nazwie, wartość i kursor przez `cursor_spans` ze stylem `field_editing`, dopełnienie przez `fill_padding`. Nieaktywne: bez tła, ze stylem `Problem` przy błędzie.
- `main_body`: zamiast `rows.resize(main_height, Line::raw(""))` dopełnić wierszami `CONTINUATION` + `fill_padding(0, width, fill)` gdy pole jest aktywne.
- Brak pola Opis (AE9): kolumna pokazuje pola krótkie i komunikat `NO_TEXT_FIELD` pod nimi.
- Przewijanie przy niskim terminalu: policzyć wiersz fokusu w kolumnie i przesunąć widok, by był widoczny.
- Usunąć martwy kod paska, jeśli po zmianie nic go nie używa.

**Patterns to follow:** `value_lines`, `fill_padding`, `cursor_spans` w `src/tui/form/field.rs`; `render_fields` i `field_lines` w `src/tui/chooser.rs` (przewijanie po `focus_line`).

**Test scenarios:** (bufor z `TestBackend`, odczyt `bg` komórek, jak istniejące testy tła z #24)
- Covers AE1. Wpis z polami Ticket, Tytuł, Repo, Opis: cztery nagłówki lub wiersze w czterech różnych wierszach `y`, jedno pod drugim; żadne dwa pola nie dzielą wiersza.
- Covers AE1. `↓` z Ticketu daje kolejno fokus na Tytuł, Repo, Opis; `Tab` i `Shift+Tab` idą tą samą ścieżką (`tests/tui_state.rs`).
- Covers AE2. Puste aktywne pole Opis, wysokość formularza 30: komórki `text_width` we wszystkich wierszach pola, aż do dołu obszaru, mają `bg` z `field_editing`.
- Covers AE3. Opis z dwiema liniami: wiersze pod tekstem mają to samo `bg` co wiersze tekstu.
- Covers AE4. Formularz odpowiedzi na pauzę (`waiting.rs`) z aktywnym polem wieloliniowym: cały prostokąt ma tło.
- Wiele pól wieloliniowych: aktywne ma tło w całym prostokącie, zwinięte nie ma tła i nie ma pustego tła pod sobą.
- Covers AE9. Workflow bez pola wieloliniowego: widać Ticket, Tytuł, Repo jedno pod drugim i komunikat `brak pola tekstowego w tym workflow` pod nimi.
- R9. Aktywne pole jednolinijkowe: tło do ostatniej komórki `text_width`, `> ` bez tła (jak w #24).
- R10. Pole bez focusu: żadna komórka nie ma `field_editing`. `Styles::new(true)`: żadna komórka nie ma `bg`.
- Niski terminal (wysokość 8, cztery pola): fokus na ostatnim polu krótkim jest widoczny w buforze; brak paniki; Opis ma co najmniej 1 wiersz.
- Brak paniki przy szerokości 1–3.
- Fokus błędu: przy wysłaniu z kilkoma niepoprawnymi polami fokus idzie na pierwsze niepoprawne pole w nowej kolejności (`submit` chodzi po `focus_order_of`).

**Verification:** w buforze każde pole leży we własnym wierszu, kolejność `↓` zgadza się z kolejnością rysowania, a aktywne pole wieloliniowe ma tło w całym prostokącie.

---

### U2. Kolejność pól własnych w `new_task.rs`

**Goal:** Pola wpisu workflow idą w kolejności: Ticket, Tytuł, Repo, inne wejścia, Opis.

**Requirements:** R1, R4, R5, R6; AE1, AE10.

**Dependencies:** U1 (nawigacja wg deklaracji).

**Files:**
- Modify: `src/tui/task_app/new_task.rs` (`workflow_item`, `label_fields`)
- Test: `tests/tui_state.rs`

**Approach:**
- W `workflow_item` zbudować pola przez `workflow_input_fields`, potem połączyć z `WorkflowInputView` (ta sama długość i kolejność) i ułożyć: `label_fields()`, pola wejść typu `Repository`, pozostałe wejścia w kolejności deklaracji bez pola Opis, na końcu pole Opis.
- Pole Opis wybrać tą samą regułą co `Chooser::main_field` (pierwsze wymagane pole wieloliniowe, inaczej pierwsze wieloliniowe). Wydzielić regułę do jednej funkcji, żeby `new_task.rs` i `chooser.rs` nie miały dwóch kopii.
- Wejście `Repository` bez listy repozytoriów to zwykły tekst (`workflow_input_fields`). Nadal idzie jako Repo.
- Workflow z własnym wejściem `ticket_id` nie zmienia się: to zwykłe pole, bez limitu.
- Uwaga dla implementera: formularz otwiera się dziś na pierwszym wejściu workflow (komentarz „After the inputs"). Po zmianie otwiera się na Ticket. Zaktualizować komentarz. `Chooser::rebuilt` i `carry_over` łączą pola po nazwie, więc zmiana kolejności nie psuje szkicu.
- Wpis `ścieżka…` (`path_item`): pole `ścieżka` ma iść pierwsze, potem Ticket i Tytuł, bo `path_item` je dziś dołącza (`tests/tui_state.rs`, asercja `[PATH_FIELD, "ticket", TICKET_FIELD, TITLE_FIELD]`). Sprawdzić, czy kolejność tego wpisu też ma spełniać R1. Jeśli tak, ułożyć `ścieżka`, Ticket, Tytuł, potem `ticket`.

**Patterns to follow:** `workflow_item`, `path_item`, `workflow_input_fields` w `src/tui/input_form.rs`.

**Test scenarios:**
- Covers AE10. Workflow z wejściami `repository`, `dry_run`, `prompt`: nazwy pól po kolei to `Ticket`, `Tytuł`, `repository`, `dry_run`, `prompt`.
- Workflow bez wejścia repozytorium: Ticket, Tytuł, pozostałe wejścia, Opis.
- Workflow bez wejścia wieloliniowego: Ticket, Tytuł, Repo, pozostałe (Opis nie istnieje, nie ma pustego miejsca).
- Wejście `ticket_id` typu string: stoi wśród zwykłych wejść i nie ma limitu 15.
- Dwa pola wieloliniowe, pierwsze opcjonalne, drugie wymagane: wymagane jest ostatnie (Opis).
- Zmiana kolejności nie zmienia wartości: `workflow_input_values` i `submission.values` odczytują pola po nazwie (istniejące testy wysyłania przechodzą po aktualizacji oczekiwanej kolejności).
- Szkic po `rebuilt`: wartość wpisana w `prompt` zostaje po odświeżeniu katalogu.

**Verification:** kolejność pól w każdym wpisie zgadza się z R1 i R4. Testy wysyłania zadania przechodzą.

---

### U3. Limit znaków w polu i stałe limitów

**Goal:** `ChooserField` może mieć limit znaków. Ticket ma 15, Tytuł ma 50. Tytuł dzieli stałą z dyrygentem.

**Requirements:** R12, R13, R14, R15, R17, R18; AE5, AE6, AE7.

**Dependencies:** brak.

**Files:**
- Modify: `src/tui/chooser.rs` (`ChooserField`, `edit`, `paste`)
- Modify: `src/tui/task_app/new_task.rs` (`label_fields`, stałe)
- Modify: `crates/narsor-protocol/src/` (nowa stała), `crates/narsor-engine/src/domain/task.rs` (reeksport)
- Test: `tests/tui_state.rs`, `tests/tui_form.rs`, `tests/conductor_title.rs`

**Approach:**
- Dodać pole `max_chars: Option<usize>` i `pub fn limited(self, max: usize) -> Self`. Limit liczy `value().chars().count()`.
- `edit`, wstawianie znaku: gdy limit jest osiągnięty, znak jest zjadany (klawisz „użyty", bez zmiany wartości, bez wysyłki formularza). Backspace i Delete działają zawsze.
- `paste`: po `pasted_text` obciąć wynik do `max - znaki w polu` znaków (`chars().take(n)`), nie bajtów. Pusty wynik to `false`, jak dziś.
- Wartości ustawione programowo (`with_value`, `proposed`, `carry_over`) nie są obcinane: limit pilnuje tylko tego, co wpisuje operator (R17). Szkic może więc być dłuższy niż limit. Wolne miejsce liczyć przez `saturating_sub`, żeby nie było paniki. Przy takiej wartości wpisanie znaku jest odrzucone, a Backspace działa.
- Nowa linia w polu jednolinijkowym już jest ignorowana (`_ if newline => return false`). Nie zmieniać. Test ma potwierdzić, że formularz nie jest wysłany.
- Stałe: `TICKET_MAX_CHARS = 15` w `new_task.rs`. Formularz używa stałej `GENERATED_TITLE_MAX_CHARS` z `narsor-protocol` (przeniesionej, bez zmiany nazwy). `label_fields()` używa `.limited(...)` dla obu pól.
- Nie ruszać `crates/narsor-engine` poza reeksportem stałej. Limity serwera 64 i 200 zostają.

**Patterns to follow:** budowniczy `required`, `described` w `ChooserField`; `paste` i `pasted_text` w `src/tui/chooser.rs` i `src/tui/input_form.rs`.

**Test scenarios:**
- Covers AE5. Ticket z 15 znakami, wpisanie 16. znaku: wartość bez zmian, długość 15.
- Covers AE6. Wklejenie do pustego Tytułu tekstu z dwiema liniami i 80 znakami: pole ma jedną linię, 50 znaków, bez `\n`.
- Covers AE7. `Ctrl+J`, `Shift+Enter`, `Alt+Enter` w Tytule: wartość bez zmian, `ChooserAction::None`, brak wysyłki.
- R15. 15 polskich znaków (`ąęłóśżźćń…`) mieści się w Tickecie; 16. znak jest odrzucony. Wartość ma więcej niż 15 bajtów.
- Wklejenie przy częściowo pełnym polu (10 znaków w polu, wklejone 20): dodano 5 znaków, razem 15.
- Wklejenie w środek tekstu przy limicie: wartość nie przekracza limitu, kursor stoi za wstawionym tekstem.
- Wklejony tekst z `\r\n` i tabulatorami zamienia się na jedną linię i jest obcięty (Z8).
- Pole wieloliniowe (Opis) i pola workflow nie mają limitu: wklejenie 1000 znaków działa.
- Backspace przy polu pełnym usuwa znak i pozwala wpisać nowy.
- Stała: test w `tests/conductor_title.rs` albo nowy test sprawdza, że formularz i dyrygent używają tej samej wartości 50.

**Verification:** żadna ścieżka wejścia (klawisz, wklejenie) nie daje w Ticket więcej niż 15 znaków ani w Tytule więcej niż 50. Crate `narsor-engine` buduje się z reeksportowaną stałą.

---

### U4. Brak zmiany backendu i testy regresji

**Goal:** Udowodnić, że limity 15 i 50 nie wyciekły poza formularz.

**Requirements:** R17; AE8.

**Dependencies:** U3.

**Files:**
- Test: `tests/task_labels.rs`, `tests/tui_state.rs`

**Approach:**
- Brak zmian w kodzie produkcyjnym. Tylko testy.
- Istniejące testy `tests/task_labels.rs` sprawdzają granicę 64 i 200 znaków. Zostają bez zmian i muszą przejść.

**Test scenarios:**
- Covers AE8. Tworzenie zadania przez API z ticketem o 20 znakach: zadanie powstaje.
- Ticket o 64 znakach przez API: przyjęty; 65 znaków: odrzucony (bez zmian).
- Tytuł o 200 znakach przez API: przyjęty; 201: odrzucony (bez zmian).
- `src/cli/remote.rs`: ticket 20 znaków przechodzi bez obcięcia (test CLI w `tests/cli_task_text.rs`, jeśli istnieje pasujący przypadek; inaczej `Test expectation: none -- brak zmiany kodu CLI`).
- Formularz TUI wysyła ticket i tytuł dokładnie tak, jak wpisano (po limicie): `launch.ticket` i `launch.title` w `CreateTaskRequest`.

**Verification:** wszystkie istniejące testy backendu przechodzą bez zmian oczekiwań.

---

## Risks

- **Istniejące testy zakładają pasek i kolejność „krótkie, potem długie".** Testy w `tests/tui_state.rs` i `tests/tui_render.rs` o `inputs_layout` i o kolejności `[PATH_FIELD, "ticket", TICKET_FIELD, TITLE_FIELD]` wymagają aktualizacji. To zmiana oczekiwana, nie regresja. Zmienić oczekiwania i przejrzeć każdy przypadek.
- **Fokus startowy.** Formularz otwiera się na Ticket zamiast na pierwszym wejściu workflow. Workflow z wymaganym wejściem (np. `ticket_id`) zaczyna na polu opcjonalnym. To skutek R1. Origin to akceptuje (AE1).
- **Przewijanie kolumny a wysokość Opisu.** Zła arytmetyka wysokości daje Opis o wysokości 0 albo wiersz pod krawędzią. Testy niskiego terminala (U1) to pilnują.
- **Przeniesienie stałej między crate'ami.** Zły reeksport psuje `narsor-engine`. Zbudować cały workspace po U3.

---

## Deferred to Implementation

- Dokładny kształt funkcji wspólnej dla reguły „pole Opis" (`main_field` i `new_task.rs`).
- Dokładny wygląd wiersza pola krótkiego: `nazwa: wartość` albo `> wartość` z nazwą w nagłówku. Ma spełniać R2 i R9.
- Czy `render_status` zostaje jako osobna linia, czy opis aktywnego pola przechodzi do wiersza pola. Zależy od wysokości w niskim terminalu.

---

## Origin Coverage

- Problem Frame, Summary, Scope Boundaries i Key Decisions z origin są przeniesione. Decyzja użytkownika (limity tylko w TUI) to U3 i U4.
- Z1 (Temat = Tytuł), Z3 (Repo = `repository`), Z4 (Opis = główne pole tekstowe), Z5 (pozostałe wejścia po Repo): przyjęte bez zmian.
- Z6 (przycinanie białych znaków): bez zmian. Serwer przycina, formularz liczy znaki wpisane.
- Z7 (licznik opcjonalny): licznika nie ma. Decyzja operatora z przeglądu PR #29.
- Z8 (nowa linia to spacja): U3.
- Pytania odroczone do planowania: pasek znika całkowicie (Key Technical Decisions); wysokość w niskim terminalu (przewijanie kolumny); przyczyna tła tylko pod tekstem (Problem Frame: `resize` z pustymi wierszami); limit i wklejanie (U3).
- Brak pytań blokujących.
