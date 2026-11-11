---
date: 2026-11-11
topic: tui-naglowek-i-zakladki-szczegolow-taska
---

# Nagłówek i belka zakładek w szczegółach taska

## Summary

Nagłówek szczegółów taska traci tło i dostaje nową kolejność wierszy: pełny tytuł (tylko gdy ramka go skraca), wiersze workspace'ów, status, koszt. Wiersze workspace'ów mają separatory ` · ` i nie pokazują `—` bez PR. Status nie pokazuje `bez slotu`. Belka zakładek stoi między dwiema liniami w kolorze ramki, które łączą się z ramką. *Rozmowa* jest pierwszą zakładką. Znacznik śledzenia przechodzi z belki na linię nad listą kroków w *Rozmowie*, w krótkiej formie z `[f]`. Tekst w pigułkach na ramce ma zawsze kolor czarny.

---

## Problem Frame

Widok szczegółów taska rysuje dziś `src/tui/task_app/detail.rs` (funkcje `render_view`, `header`, `tab_bar`) i `src/tui/task_app/workspace_rows.rs`. Obecny wygląd ma kilka wad:

- Nagłówek i belka zakładek mają granatowe tło (`Styles::task_header_background`). Tło odcina się od reszty panelu i od ramki.
- Nad nagłówkiem zawsze jest pusty wiersz (`lines.insert(0, Line::default())`). Gdy ramka mieści cały tytuł, ten wiersz to tylko strata miejsca.
- Kolejność to: tytuł, status, workspace'y, koszt. Workspace'y opisują, *co* task zmienia. Status opisuje, *gdzie* task jest. Operator chce najpierw widzieć tytuł i workspace'y, a potem status.
- Wiersz workspace'u bez PR kończy się znakiem `—`. Ten znak nie niesie informacji. Kolumny w wierszu dzieli tylko spacja, więc repozytorium, gałąź, statystyki i PR zlewają się.
- Status zakończonego taska pokazuje `bez slotu`. Ta informacja wynika ze stanu taska (zakończony, pauza, błąd, czeka na człowieka), więc jest zbędna.
- Belkę zakładek od nagłówka oddziela czarna linia (`task_tabs_divider`), a pod belką jest wiersz z podkreśleniem tylko pod aktywną zakładką. Belka nie łączy się z ramką panelu.
- Znacznik śledzenia (`↧ śledzi · f wyłącz` / `⏸ ręcznie · f śledź`) stoi na końcu belki zakładek. Ten znacznik dotyczy tylko *Rozmowy*, a zajmuje miejsce w belce wspólnej dla wszystkich zakładek.
- *Rozmowa* jest drugą zakładką, a operator używa jej najczęściej.

---

## Requirements

**Tło**
- R1. Nagłówek szczegółów taska nie ma tła. Wiersze nagłówka stoją na tle panelu.
- R2. Belka zakładek (obie linie i wiersz nazw) nie ma tła.
- R3. Panel odpowiedzi (`answer_panel_background`) zostaje bez zmian. Ta zmiana dotyczy tylko nagłówka i belki zakładek.

**Pigułki na ramce**
- R4. Tekst w pigułkach na górnej krawędzi ramki (ticket, tytuł, krótki id taska) ma zawsze kolor czarny, niezależnie od fokusu, stanu `stale` i roli ramki.
- R5. Tryb bez koloru (`no_color`) zostaje bez zmian: pigułka to `┤ tekst ├` w stylu roli.

**Kolejność i wiersze nagłówka**
- R6. Wiersze nagłówka stoją w tej kolejności: (1) pełny tytuł, (2) wiersze workspace'ów, (3) status, (4) koszt, (5) powiadomienie, (6) belka zakładek.
- R7. Wiersz pełnego tytułu jest widoczny tylko wtedy, gdy ramka skraca tytuł (`title_is_cut`). Gdy ramka mieści cały tytuł, tego wiersza nie ma i nie ma w jego miejscu pustego wiersza.
- R8. Nad pierwszym wierszem nagłówka nie ma pustego wiersza odstępu.
- R9. Wiersz kosztu jest widoczny tylko wtedy, gdy harness podał zużycie (jak dziś).
- R10. Wiersz powiadomienia (utrata runnera, niedostępny harness) jest widoczny tylko wtedy, gdy jest co powiedzieć (jak dziś). Stoi pod kosztem, nad belką zakładek.

**Wiersze workspace'ów**
- R11. Gdy workspace nie ma PR, wiersz nie pokazuje pola PR. Znak `—` znika.
- R12. Między repozytorium a gałęzią stoi separator ` · `.
- R13. Między gałęzią bazową a statystykami `+N −M` stoi separator ` · `.
- R14. Między statystykami a PR stoi separator ` · `.
- R15. Separator stoi tylko między dwoma widocznymi elementami. Gdy element jest pusty lub wąski terminal go usunął, separator przed nim też znika. Wiersz nie kończy się separatorem.
- R16. Kolumny między wierszami workspace'ów są nadal wyrównane, a kolejność skracania przy wąskim terminalu zostaje bez zmian (najpierw gałąź bazowa, potem statystyki, potem skrócenie gałęzi, potem repozytorium; nazwy i PR nie skracamy).

**Status**
- R17. Wiersz statusu nie pokazuje `bez slotu`. Pozostałe części wiersza statusu zostają bez zmian.

**Belka zakładek**
- R18. Kolejność zakładek: *Rozmowa*, *Przebieg*, *Zdarzenia*, *Dyrygent*, *Info*. Ta sama kolejność dotyczy przechodzenia między zakładkami klawiszami.
- R19. Belka zakładek ma trzy wiersze: górna linia, wiersz nazw zakładek, dolna linia.
- R20. Górna i dolna linia biegną przez całą szerokość wnętrza panelu i łączą się z bokami ramki znakami łącznika (`├` po lewej, `┤` po prawej).
- R21. Obie linie i łączniki mają kolor ramki w tej chwili: rola fokusu, gdy szczegóły mają klawiaturę, rola tła w innym przypadku, rola tła przy `stale`.
- R22. Na dolnej linii odcinek pod aktywną zakładką ma inny kolor i jest pogrubiony (`━`), tak jak dzisiejsze podkreślenie aktywnej zakładki: rola fokusu przy fokusie, w innym przypadku rola tła z pogrubieniem.
- R23. Górna linia zastępuje dzisiejszą czarną linię oddzielającą nagłówek od belki. Wiersz pod belką, w którym dziś stoi samo podkreślenie, staje się dolną linią z R19.
- R24. Na końcu wiersza nazw zakładek nie ma znacznika śledzenia *Rozmowy*.

**Znacznik śledzenia w *Rozmowie***
- R25. W zakładce *Rozmowa* górna linia paska kroków (linia nad listą kroków) pokazuje znacznik śledzenia.
- R26. Znacznik ma krótką formę: `↧ śledzi [f]`, gdy widok śledzi najnowszy krok, i `⏸ ręcznie [f]`, gdy nie śledzi. Kolory jak dziś: rola „live” dla śledzenia, rola „uwaga” dla trybu ręcznego.
- R27. Znacznik stoi przy prawym końcu linii, w linii (np. `───── ↧ śledzi [f] ─`). Gdy linia jest za krótka na znacznik, znacznika nie ma, a linia jest cała.
- R28. Klawisz `f` działa jak dziś (przełącza śledzenie).

**Bez zmian**
- R29. Tryb `stale` szarzeje elementy jak dziś. Tryb `no_color` rysuje linie i łączniki w stylu roli bez koloru.
- R30. Linki PR (OSC 8) w wierszach workspace'ów nadal działają i trafiają w tekst PR po zmianie układu wiersza.

---

## Acceptance Examples

- AE1. **Covers R6, R7, R8.** Given task z krótkim tytułem, który ramka pokazuje w całości, when operator otwiera szczegóły, then pierwszy wiersz wewnątrz ramki to pierwszy wiersz workspace'u. Nad nim nie ma pustego wiersza.
- AE2. **Covers R6, R7.** Given task z długim tytułem, który ramka skraca znakiem `…`, when operator otwiera szczegóły, then pierwszy wiersz wewnątrz ramki to pełny tytuł, pod nim są wiersze workspace'ów, potem status, potem koszt.
- AE3. **Covers R11–R15.** Given workspace z repo `orders-api`, gałęzią `feat/nar-12` na bazie `main`, zmianami `+120 −34` i PR `#42`, when nagłówek się rysuje, then wiersz to `code: orders-api · ⎇ feat/nar-12 → main · +120 −34 · #42 ↗`.
- AE4. **Covers R11, R15.** Given workspace `docs` na gałęzi `main`, bez zmian i bez PR, when nagłówek się rysuje, then wiersz to `docs: orders-docs · ⎇ main`, bez `—` i bez separatora na końcu.
- AE5. **Covers R17.** Given zakończony task, który nie trzyma slotu, when nagłówek się rysuje, then wiersz statusu nie zawiera `bez slotu`.
- AE6. **Covers R18–R22.** Given szczegóły z fokusem i aktywną *Rozmową*, when belka się rysuje, then widać:
  ```text
  ├────────────────────────────────────────────────────┤
  │  Rozmowa   Przebieg   Zdarzenia   Dyrygent   Info  │
  ├──━━━━━━━───────────────────────────────────────────┤
  ```
  Obie linie mają kolor fokusu. Odcinek `━━━━━━━` pod *Rozmową* jest pogrubiony i ma kolor aktywnej zakładki.
- AE7. **Covers R24–R27.** Given *Rozmowa*, która śledzi najnowszy krok, when zakładka się rysuje, then wiersz nazw zakładek nie ma znacznika, a górna linia paska kroków kończy się `↧ śledzi [f] ─`. Po naciśnięciu `f` znacznik to `⏸ ręcznie [f]`.
- AE8. **Covers R1, R2.** Given tryb kolorowy, when szczegóły się rysują, then komórki nagłówka i belki zakładek nie mają granatowego tła.

---

## Success Criteria

- Nagłówek i belka zakładek wyglądają jak część panelu: brak tła, belka łączy się z ramką.
- Gdy ramka mieści tytuł, nagłówek jest niższy o jeden wiersz niż dziś.
- Operator czyta wiersz workspace'u bez zgadywania, gdzie kończy się repo, gałąź i statystyki.
- Operator widzi stan śledzenia tam, gdzie ma znaczenie: nad listą kroków *Rozmowy*.
- Planowanie nie musi wymyślać kolejności wierszy, separatorów ani wyglądu belki.

---

## Scope Boundaries

- Treść zakładek (*Przebieg*, *Zdarzenia*, *Dyrygent*, *Info*) bez zmian.
- Lista tasków, panel odpowiedzi i formularze bez zmian.
- Logika śledzenia w *Rozmowie* i klawisz `f` bez zmian. Zmienia się tylko miejsce i tekst znacznika.
- Znacznik `↧ podąża` / `· ręcznie` zakładki *Przebieg* — patrz Pytania otwarte (Q1). Do decyzji zostaje tam, gdzie jest.
- Widok szczegółów taska w historii (`history.rs`) dostaje te same zmiany, gdy używa tego samego `render_view`. Nie ma osobnego projektu dla historii.

---

## Key Decisions

- Wiersz pełnego tytułu nie zostawia pustego miejsca, gdy jest ukryty: cel to krótszy nagłówek.
- `bez slotu` usuwamy tylko z nagłówka szczegółów. Inne miejsca (CLI, lista) nie są częścią tej zmiany.
- Belka zakładek dostaje własne linie w kolorze ramki zamiast tła: struktura zastępuje kolor.

---

## Założenia do potwierdzenia

- Z1. „Na sztywno czarny” (R4) oznacza kolor RGB `(0, 0, 0)`, nie kolor ANSI `Black`. Kolor ANSI `Black` motyw terminala może zmienić na szary. Dziś pigułka ma `Color::Black` (ANSI). Bez koloru (`no_color`) nic się nie zmienia.
- Z2. „Pola opisujące szczegóły na ramce” to pigułki na górnej krawędzi ramki: ticket, tytuł i krótki id. Znacznik ładowania i zwykły tytuł bez pigułki nie są częścią tej zmiany.
- Z3. Separator między nazwą workspace'u (`code:`) a repozytorium zostaje bez zmian (spacja). Prośba wymienia tylko repo–gałąź, baza–statystyki i statystyki–PR.
- Z4. Znak `⎇` przed gałęzią i strzałka `→` między gałęzią a bazą zostają.
- Z5. Gdy workspace nie ma statystyk, a ma PR, między gałęzią (lub bazą) a PR stoi jeden separator ` · ` (R15).
- Z6. Wiersz powiadomienia (utrata runnera, niedostępny harness) stoi pod kosztem, nad belką zakładek. Prośba nie wymienia tego wiersza.
- Z7. Domyślna zakładka po otwarciu szczegółów zostaje taka jak dziś. Zmienia się tylko kolejność zakładek na belce i przy przechodzeniu klawiszami.
- Z8. „Linia nad listą kroków” to dzisiejsza górna linia paska kroków w *Rozmowie* (`render_step_strip`), nie dolna linia belki zakładek. Obie linie zostają osobno.
- Z9. Forma uproszczona znacznika to `↧ śledzi [f]` i `⏸ ręcznie [f]`, przy prawym końcu linii.
- Z10. Łączniki belki (`├`, `┤`) to zwykłe łączniki cienkiej linii, pasujące do zaokrąglonej ramki. Przy fokusie ramka i linie mają ten sam kolor fokusu.

---

## Pytania otwarte dla użytkownika

- Q1. **Belka zakładek / Scope Boundaries.** Co robimy ze znacznikiem `↧ podąża` / `· ręcznie` zakładki *Przebieg* na końcu belki?
  - (zalecane) Zostaje na końcu belki, tylko dla *Przebiegu*. Zmiana dotyczy tylko znacznika *Rozmowy*.
  - Usuwamy go z belki i przenosimy na linię nad osią kroków w *Przebiegu*, w tej samej krótkiej formie z `[f]`.
  - Usuwamy go całkiem.
- Q2. **Znacznik śledzenia w *Rozmowie*.** Czy dolna linia belki zakładek i górna linia paska kroków w *Rozmowie* mają zostać osobno?
  - (zalecane) Zostają osobno. Znacznik stoi na górnej linii paska kroków.
  - Łączymy je w jedną linię: dolna linia belki jest linią nad listą kroków, a znacznik stoi przy jej prawym końcu.

---

## Next Steps

→ `/ce-plan` dla planu implementacji. Testy renderowania w `tests/integration/tui_render.rs` opisują dzisiejszy układ nagłówka i belki, więc plan musi je zaktualizować.
