---
date: 2026-10-19
topic: focus-po-zatwierdzeniu-akcji
---

# Focus wraca na szczegóły taska po zatwierdzeniu akcji

## Summary

Gdy operator zatwierdzi akcję w szczegółach taska (np. odpowiedź agentowi) klawiszem `Enter`, focus zostaje w szczegółach tego taska. Dziś focus przeskakuje na listę tasków.

---

## Problem Frame

Operator odpowiada agentowi w panelu odpowiedzi i naciska `Enter`. Odpowiedź jest wysłana. Focus przechodzi na listę tasków. Operator chce zwykle zobaczyć dalszy przebieg tego samego taska. Musi więc wrócić do szczegółów klawiszem (`Enter` lub `Tab`).

Przyczyna (zweryfikowana w kodzie): po udanym wysłaniu odpowiedzi kod w `src/tui/task_app/waiting.rs` (ok. linii 1556) ustawia `Focus::List`, wyłącza `panel_focus` i wraca na oś czasu (`back_to_axis`).

---

## Requirements

- R1. Po udanym zatwierdzeniu akcji w szczegółach taska focus pozostaje w szczegółach tego samego taska. Lista tasków nie przejmuje focusa.
- R2. Zaznaczony task na liście się nie zmienia. Szczegóły nadal pokazują ten sam task.
- R3. Szczegóły wracają na oś czasu (*Przebieg*), tak jak dziś. Focus nie zostaje w polu odpowiedzi ani na liście akcji, bo akcja jest już obsłużona.
- R4. Gdy zatwierdzenie się nie uda (błąd, odrzucenie), focus i stan panelu nie zmieniają się. Operator może poprawić i ponowić. To zachowanie już istnieje i zostaje.
- R5. Reguła dotyczy każdej akcji zatwierdzanej z szczegółów taska, nie tylko odpowiedzi. Żadna taka akcja nie przenosi focusa na listę po sukcesie.
- R6. Pozostałe przejścia focusa nie zmieniają się: `Esc` w szczegółach nadal wraca na listę, a `Tab` nadal przełącza panele.

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Dane: task czeka na odpowiedź, focus jest w panelu odpowiedzi. Gdy operator wpisuje odpowiedź i naciska `Enter`, a wysłanie się udaje, focus jest w szczegółach tego taska, na osi czasu. Zaznaczenie listy się nie zmieniło.
- AE2. **Covers R4.** Dane: wysłanie odpowiedzi kończy się błędem. Focus zostaje w panelu odpowiedzi, a tekst odpowiedzi nie znika.
- AE3. **Covers R6.** Dane: focus w szczegółach. Gdy operator naciska `Esc`, focus przechodzi na listę.

---

## Success Criteria

- Operator odpowiada agentowi i dalej widzi ten sam task bez dodatkowego klawisza.
- Planista nie musi decydować, gdzie ma trafić focus. Opisują to R1–R3.

---

## Scope Boundaries

- Brak zmiany `Esc`, `Tab` i innych skrótów nawigacji.
- Brak zmian w widoku historii, formularzu nowego zadania i widoku repozytoriów. Mają własne przejścia focusa (`history.rs`, `new_task.rs`, `repositories.rs`).
- Brak automatycznego przechodzenia do następnego taska, który czeka na operatora (istnieje osobna akcja `NextWaiting`).

---

## Key Decisions

- Po sukcesie focus trafia na oś czasu, nie z powrotem do pola odpowiedzi. To zgodne z obecnym `back_to_axis` i z faktem, że akcja jest zakończona. Zmiana jest mała: zmienia się tylko wartość `Focus`.

## Dependencies / Assumptions

- Założenie (do potwierdzenia przez Artura): „szczegóły taska" znaczą oś czasu, a nie panel odpowiedzi. Jeśli Artur woli panel odpowiedzi (przy dalszym oczekiwaniu na operatora), R3 trzeba zmienić.
- Założenie: miejsc z `Focus::List` po akcji jest więcej niż jedno. Znaleziono jedno w `waiting.rs`. Inne ścieżki akcji (komendy operatora i właściciela) przechodzą przez to samo miejsce, ale wymaga to potwierdzenia w planie.

## Outstanding Questions

### Deferred to Planning

- [Affects R5][Technical] Czy każda akcja zatwierdzana w szczegółach przechodzi przez kod z `waiting.rs`, czy któraś ma własną ścieżkę?
- [Affects R1][Technical] Które testy w `tests/tui_state.rs` zakładają focus na liście po wysłaniu i trzeba je zmienić.
