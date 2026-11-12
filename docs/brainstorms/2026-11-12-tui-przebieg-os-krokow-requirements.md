---
date: 2026-11-12
topic: tui-przebieg-os-krokow
---

# Oś kroków w *Przebiegu*: bez tła zaznaczenia, czas przy nazwie, bez słowa stanu

## Summary

Oś kroków w zakładce *Przebieg* traci tło zaznaczonego kroku. Czas startu i czas trwania kroku stoją w pierwszym wierszu, tuż za nazwą kroku. Słowo stanu (`ok`, `w toku`, `błąd`…) znika z osi i z nagłówka wybranego kroku. Stan niesie sama ikona.

---

## Problem Frame

Zakładkę *Przebieg* rysuje `src/tui/task_app/tabs/progress.rs`. Oś kroków to lista z klamrą (`render_bracketed_list` w `src/tui/selection.rs`). Obecny wygląd ma trzy wady:

- Zaznaczony krok ma klamrę (dwie linie i pasek) i dodatkowo wypełnione tło (`Styles::selection_fill`). Klamra już pokazuje zaznaczenie. Tło dodaje szum, a w innych miejscach interfejsu (nagłówek, belka zakładek) tło zostało usunięte z tego samego powodu.
- Czas startu i czas trwania stoją w drugim wierszu kroku, razem z modelem, wysiłkiem i harnessem. Operator patrzy na nazwę kroku w pierwszym wierszu. Czas jest od niej oddalony o jeden wiersz.
- Za nazwą kroku stoi słowo stanu (`ok`, `w toku`, `błąd`, `przerwany`, `czeka na decyzję`). Ikona przed nazwą niesie tę samą informację. Słowo zajmuje miejsce, które skraca nazwę kroku. Nagłówek wybranego kroku po prawej stronie powtarza stan jeszcze raz (` · ok`).

---

## Requirements

**Tło zaznaczenia**
- R1. Zaznaczony krok na osi w *Przebiegu* nie ma wypełnionego tła. Klamra (dwie linie i pasek po lewej) zostaje bez zmian i nadal pokazuje zaznaczenie.
- R2. Zmiana dotyczy tylko osi w *Przebiegu*. Lista *Historia* i choosery zostają bez zmian, mimo że używają tej samej klamry.
- R3. Zaznaczenie nadal nie opiera się na samym kolorze. Klamra jest w roli fokusu, gdy klawisze są na osi, a w roli tła, gdy gdzie indziej (jak dziś).

**Czas przy nazwie**
- R4. Pierwszy wiersz kroku pokazuje, w tej kolejności: ikonę stanu, nazwę kroku, czas startu, czas trwania. Czas startu ma dotychczasowy format (`MM-DD HH:MM:SS`). Czas trwania ma dotychczasowy format (`short_age`).
- R5. Czas startu i czas trwania znikają z drugiego wiersza kroku. Drugi wiersz pokazuje tylko model, wysiłek i harness. Gdy krok nie ma żadnej z tych wartości, drugiego wiersza nie ma (jak dziś).
- R6. Gdy krok nie ma czasu startu (`started_at` puste), pole czasu jest puste i nie zostawia osieroconego separatora.
- R7. Gdy krok trwa (`ended_at` puste), czas trwania liczy się do chwili obserwacji (jak dziś).
- R8. Kolumny czasu są wyrównane między krokami: czas startu w jednej kolumnie, czas trwania wyrównany do prawej w drugiej. Dotyczy to także osi z tabelą zużycia (koszt, tokeny, subagenci). Kolumny zużycia stoją za kolumnami czasu.
- R9. Gdy oś jest za wąska, najpierw skraca się nazwa kroku, do dotychczasowego minimum (`MIN_LABEL`). Dopiero potem znika czas trwania, a na końcu czas startu. Nazwa kroku nigdy nie znika. Oś nie przekracza dotychczasowego udziału zakładki (70%).

**Stan kroku**
- R10. Wiersz kroku na osi nie pokazuje słowa stanu. Stan niesie ikona (`✓`, `✗`, `●`, `?`) w dotychczasowej roli koloru.
- R11. Nagłówek wybranego kroku po prawej stronie osi nie pokazuje słowa stanu. Ikona w nagłówku zostaje. Reszta nagłówka (typ akcji, decydujący, czas trwania) zostaje bez zmian.
- R12. Błąd kroku nadal ma własny wiersz `✗ <treść błędu>` w nagłówku wybranego kroku. Noty (`ponowienie oczekuje` i inne) zostają bez zmian.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Given oś w *Przebiegu* z zaznaczonym krokiem, when oś jest rysowana, then wiersze zaznaczonego kroku nie mają koloru tła, a klamra jest widoczna. Given lista *Historia* z zaznaczonym wpisem, when lista jest rysowana, then wpis ma tło jak dziś.
- AE2. **Covers R4, R5.** Given krok, który wystartował 90 sekund temu i trwa model `sonnet`, when oś jest rysowana, then pierwszy wiersz to `✓ <nazwa>  <MM-DD HH:MM:SS>  1m`, a drugi to `sonnet  high  claude-code`.
- AE3. **Covers R6.** Given krok bez czasu startu, when oś jest rysowana, then pierwszy wiersz to ikona i nazwa, bez pustego pola i bez separatora na końcu.
- AE4. **Covers R9.** Given wąska zakładka, when oś nie mieści nazwy, czasu startu i czasu trwania, then najpierw nazwa skraca się do `MIN_LABEL`, potem znika czas trwania, potem czas startu.
- AE5. **Covers R10, R11.** Given krok w toku, when oś i nagłówek są rysowane, then nigdzie nie ma tekstu `w toku`, a ikona `●` jest w roli „na żywo".

---

## Success Criteria

- Operator czyta z jednego wiersza: stan (ikona), nazwę i czas kroku. Nie szuka czasu w wierszu pod nazwą.
- Zaznaczony krok jest widoczny po klamrze, bez tła.
- Nazwy kroków są skracane rzadziej niż dziś, bo słowo stanu nie zajmuje miejsca.

---

## Scope Boundaries

- Zmiana nie dotyczy listy *Historia*, chooserów i listy tasków. Nie zmienia też wspólnej klamry w innych widokach.
- Zmiana nie dotyczy zawartości po prawej stronie osi (rozmowa kroku), poza słowem stanu w nagłówku (R11).
- Zmiana nie dodaje nowych danych do osi ani nowych klawiszy.
- Zmiana nie zmienia formatu czasu ani formatu trwania.
- Historia zamkniętego taska używa tej samej osi (`Context::HistoryAxis`). Zmiany obejmują ją tak samo jak oś taska w toku.

---

## Key Decisions

- Tło znika tylko na osi *Przebiegu*, a nie we wspólnej klamrze. Powód: *Historia* i choosery nie były w zakresie prośby, a ich zmiana to osobna decyzja o wyglądzie.
- Model, wysiłek i harness zostają w drugim wierszu. Powód: prośba mówi tylko o czasie startu i czasie trwania. Przeniesienie reszty zrobiłoby pierwszy wiersz za szeroki.
- Słowo stanu znika także z nagłówka wybranego kroku. Powód: nagłówek stawia słowo „obok nazwy kroku" i ikona stoi tuż przed nazwą, więc prośba pasuje do obu miejsc.

---

## Dependencies / Assumptions

Brak pytań do osoby w tej sesji. Przyjęto najbezpieczniejsze założenia:

- Założenie 1: „extra background colour" to `selection_fill` w klamrze. Sprawdzono w kodzie (`draw_line` w `src/tui/selection.rs`). Klamra i pasek zostają.
- Założenie 2: „step state next to its name" dotyczy osi (pewne) i nagłówka wybranego kroku (R11). Gdy osoba chce zostawić słowo w nagłówku, wystarczy usunąć R11.
- Założenie 3: Stany `Failed` i `Interrupted` mają dziś tę samą ikonę `✗`. Po zmianie osoba odróżnia je tylko po treści błędu w nagłówku (R12). Osoba ma wybrać, czy `Interrupted` dostaje własną ikonę. Ta zmiana jest poza zakresem, bo prośba mówi, że ikona wystarcza.
- Założenie 4: Kolejność skracania przy wąskiej osi (R9) to najmniejsza zmiana zachowania. Czas trwania znika przed czasem startu, bo niesie mniej informacji niż moment startu.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R8][Technical] Jak zachować jedną kolumnę czasu, gdy oś ma tabelę zużycia, a szerokość osi rośnie do limitu 70%.
- [Affects R1][Technical] Jak wyłączyć wypełnienie tylko dla osi *Przebiegu*, bez zmiany *Historii* i chooserów.
- [Affects R3][Needs research] Czy bez tła klamra w roli tła (klawisze poza osią) nadal jest dobrze widoczna w trybie `no_color`.
