---
date: 2026-11-12
topic: tui-przebieg-odstepy-i-szersza-os
---

# Oś kroków w *Przebiegu*: większe odstępy między danymi i szersza lista

## Summary

Dane obok nazwy kroku w zakładce *Przebieg* (czas startu, czas trwania, koszt, tokeny, subagenci) dostają większe odstępy. Oś kroków dostaje większy udział szerokości zakładki. Rozmowa kroku dostaje mniejszy udział.

---

## Problem Frame

Oś kroków rysuje `src/tui/task_app/tabs/progress.rs`. Dziś kolumny danych za nazwą kroku stoją co dwie spacje (`COLUMN_GAP`). Czas startu, czas trwania i wartości zużycia zlewają się w jeden ciąg. Operator nie widzi szybko, gdzie kończy się jedna wartość, a zaczyna następna.

Oś ma też limit szerokości 70% zakładki. Gdy oś ma kolumny zużycia, limit zmusza do skracania nazw kroków albo do ukrywania czasu. Większy odstęp zwiększa to ciśnienie, bo kolumny zajmują więcej miejsca.

---

## Requirements

**Odstępy**
- R1. Odstęp między kolumnami danych na osi wynosi 3 spacje (dziś 2). Dotyczy to wszystkich kolumn za nazwą kroku: czas startu, czas trwania i kolumny zużycia (koszt, tokeny, subagenci).
- R2. Odstęp między nazwą kroku a pierwszą kolumną danych wynosi także 3 spacje.
- R3. Wiersz kroku nadal kończy się ostatnią wartością. Za nią nie stoją spacje.
- R4. Kolumny nadal są wyrównane między krokami, jak dziś. Wiersz szczegółów (druga linia kroku: model, wysiłek, harness) zachowuje swoje wcięcie i separator.

**Szerokość osi**
- R5. Oś może zająć do 75% szerokości zakładki (dziś 70%). Rozmowa kroku zachowuje co najmniej 25% (dziś 30%).
- R6. Oś nadal zajmuje tylko tyle miejsca, ile potrzebują jej dane. Nowy limit nie rozciąga osi, gdy dane są wąskie.
- R7. Gdy oś nadal jest za wąska, kolejność skracania zostaje bez zmian: najpierw nazwa kroku (do `MIN_LABEL`), potem czas trwania, na końcu czas startu. Nazwa kroku nigdy nie znika.
- R8. Zmiana dotyczy osi taska w toku i osi historii zamkniętego taska (ta sama oś).

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Given krok z czasem startu, czasem trwania i zużyciem, when oś jest rysowana, then między nazwą a czasem startu, między czasem startu a czasem trwania i między kolejnymi kolumnami zużycia są po 3 spacje.
- AE2. **Covers R3.** Given krok bez zużycia, when oś jest rysowana, then wiersz kończy się czasem trwania, bez spacji na końcu.
- AE3. **Covers R5, R6.** Given zakładka o szerokości 200 kolumn i krok z pełnym zużyciem, when oś jest rysowana, then oś zajmuje nie więcej niż 150 kolumn, a rozmowa co najmniej 50 (z przerwą między nimi). Given zakładka z krótkimi nazwami i bez zużycia, when oś jest rysowana, then oś jest wąska jak dziś.
- AE4. **Covers R7.** Given wąska zakładka, when oś nie mieści danych, then nazwa kroku skraca się do `MIN_LABEL`, potem znika czas trwania, potem czas startu.

---

## Success Criteria

- Operator odróżnia wartości w wierszu kroku bez wpatrywania się.
- Nazwy kroków i czasy znikają rzadziej niż po samym zwiększeniu odstępów, bo oś ma więcej miejsca.
- Rozmowa kroku pozostaje czytelna na typowej szerokości terminala.

---

## Scope Boundaries

- Zmiana nie dotyczy formatu czasu, formatu trwania ani kolejności kolumn.
- Zmiana nie dotyczy listy *Historia*, chooserów ani innych zakładek.
- Zmiana nie dodaje nowych danych ani klawiszy.
- Zmiana nie zmienia stałego limitu osi bez zużycia (`AXIS_MAX_WIDTH`) ani klamry zaznaczenia.
- Zmiana nie wprowadza ustawienia szerokości przez operatora.

---

## Key Decisions

- Wartości liczbowe (3 spacje, 75%) to najmniejsza zmiana, która widocznie poprawia czytelność. Powód: prośba nie podaje liczb, a duży skok (np. 4 spacje, 80%) zabiera rozmowie za dużo miejsca.
- Jeden odstęp dla wszystkich kolumn. Powód: różne odstępy dla czasu i zużycia komplikują wyrównanie i nie wynikają z prośby.

---

## Dependencies / Assumptions

Brak pytań do osoby w tej sesji. Przyjęto najbezpieczniejsze założenia:

- Założenie 1: „dane obok nazwy kroku" to czas startu, czas trwania i kolumny zużycia na osi. Wiersz szczegółów (model, wysiłek, harness) ma już własny separator i zostaje bez zmian.
- Założenie 2: „poszerz całą listę" to wyższy limit udziału osi w zakładce, a nie stała szerokość. Oś nadal rośnie tylko do potrzeb danych (R6).
- Założenie 3: Liczby 3 spacje i 75% to wartość startowa. Osoba może ją zmienić przy przeglądzie. Zmiana liczby nie zmienia reszty wymagań.
- Założenie 4: Dokument `docs/brainstorms/2026-11-12-tui-przebieg-os-krokow-requirements.md` (czas przy nazwie, bez tła i bez słowa stanu) obowiązuje dalej. Ten dokument zmienia tylko odstępy i limit szerokości (R9 tamtego dokumentu: limit 70% zmienia się na 75%).

---

## Outstanding Questions

### Deferred to Planning

- [Affects R5][Technical] Czy rozmowa kroku przy 25% szerokości zawija wpisy czytelnie na wąskich terminalach (60–80 kolumn). Planowanie sprawdza to i może zawęzić nowy limit do większych szerokości.
- [Affects R1][Technical] Które testy i opis w `docs/user/tui.md` (limit 70%, odstęp dwóch spacji) wymagają aktualizacji.
