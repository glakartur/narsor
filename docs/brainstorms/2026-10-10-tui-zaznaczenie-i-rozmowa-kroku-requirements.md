---
date: 2026-10-10
topic: tui-zaznaczenie-i-rozmowa-kroku
---

# TUI: wejście w rozmowę kroku i zaznaczenie bez inwersji kolorów

## Summary

Na *Przebiegu* `Enter`/`→` przenosi fokus z osi kroków do rozmowy wybranego kroku. Tam `j`/`k` wybierają kolejne wpisy, a `Esc`/`←` wraca na oś. W całym TUI reverse video przestaje oznaczać zaznaczenie. Zastępuje je klamra: pozioma kreska nad i pod elementem oraz osobna pionowa kreska z lewej. Klamra jest gruba i cyan w panelu z fokusem, cienka i szara bez niego. Rozmowy i *Zdarzenia* dostają samą lewą kreskę.

---

## Problem Frame

Na *Przebiegu* klawisze `j`/`k` zawsze przesuwają krok na osi. Rozmowa wybranego kroku po prawej dostaje tylko pozostałe klawisze (przewijanie stronami, rozwinięcie, widok surowy). Wpisu w rozmowie kroku nie da się więc wybrać strzałką, a przez to nie da się go też rozwinąć ani przełączyć na surowy. Na zakładce *Rozmowa* to działa, a w rozmowie jednego kroku nie.

Zaznaczenie na liście zadań, na osi kroków, w *Zdarzeniach*, w historii, w chooserach i w panelu „Czeka na Ciebie” to reverse video. Pełne pasy odwróconych kolorów wyglądają topornie i gubią kolory ról w zaznaczonym wierszu. Dużo lepiej działa kreska `┃` w kolorze Focus, którą transkrypt już dziś oznacza wybrany wpis.

---

## Requirements

**Wejście w rozmowę kroku**
- R1. Na *Przebiegu* fokus ma dwa poziomy: oś kroków i rozmowa wybranego kroku. Domyślnie po wejściu w szczegóły jest na osi.
- R2. `Enter` albo `→` na osi przenosi fokus do rozmowy wybranego kroku. Gdy krok nie ma rozmowy do pokazania (brak zapisu, rozmowa nieutworzona, jeszcze się wczytuje), fokus zostaje na osi.
- R3. W rozmowie kroku `j`/`k`/`↑`/`↓` wybierają poprzedni lub następny wpis. Działają tam też te same klawisze co na zakładce *Rozmowa*: przewijanie stronami, początek i koniec, rozwinięcie wpisu, widok surowy wpisu i całości.
- R4. `Esc` albo `←` w rozmowie kroku wraca na oś z tym samym wybranym krokiem. Dopiero `Esc` na osi działa tak jak dziś, czyli wraca na listę zadań.
- R5. Gdy fokus jest w rozmowie kroku, oś nie zmienia wybranego kroku sama z siebie. Śledzenie bieżącego kroku (`f`) nie przenosi zaznaczenia, dopóki operator nie wróci na oś.
- R6. Pasek skrótów i pomoc (`?`) pokazują klawisze właściwe dla poziomu, na którym jest fokus.

**Klamra zaznaczenia**
- R7. Reverse video nie oznacza zaznaczenia nigdzie w TUI.
- R8. Zaznaczony element w listach oznacza klamra złożona z trzech części: poziomej kreski w wierszu nad elementem, poziomej kreski w wierszu pod nim i pionowej kreski z lewej przy każdej linii elementu. Pionowa kreska nie łączy się z poziomymi: nie ma narożników, a kreski poziome zaczynają się za kolumną pionowej.
- R9. Klamra dotyczy listy zadań, listy historii, osi kroków, chooserów (menu akcji, wybór workflow, harnessu, repozytoriów, pól wejść) oraz listy akcji w panelu „Czeka na Ciebie”.
- R10. Elementy tych list rozdziela stały wiersz odstępu, także nad pierwszym i pod ostatnim elementem. Klamra rysuje się w tych odstępach, więc ruch kursora nie przesuwa żadnego wiersza. Zadanie na liście zajmuje tym samym trzy wiersze: dwie linie zadania i odstęp.
- R11. Przewijanie i nawigacja przechodzą o cały element. Klamra zaznaczonego elementu zawsze mieści się w widoku w całości.
- R12. W panelu z fokusem klamra jest gruba (linie pogrubione) i w roli Focus. W panelu bez fokusu zaznaczenie zostaje widoczne jako ta sama klamra, ale cienka i szara.
- R13. Zaznaczony wiersz zachowuje kolory ról swojej treści (ikona stanu, marker uwagi, etykieta). Klamra ich nie nadpisuje.
- R14. Bez kolorów (`NO_COLOR`) zaznaczenie i fokus dalej da się rozpoznać, bo różnią się kształtem (klamra) i grubością linii.

**Lewa kreska w rozmowach i Zdarzeniach**
- R15. Transkrypty (zakładka *Rozmowa*, rozmowa kroku na *Przebiegu*, czat dyrygenta) i lista *Zdarzeń* oznaczają wybrany element samą lewą kreską `┃`, bez kresek poziomych i bez wierszy odstępu.
- R16. Lewa kreska stosuje tę samą regułę fokusu co klamra: gruba i cyan, gdy jej panel lub poziom ma fokus, a cienka i szara, gdy nie ma. W rozmowie kroku wybrany wpis ma kreskę cyan tylko wtedy, gdy fokus jest w rozmowie.

**Panel „Czeka na Ciebie”**
- R17. Gdy akcje z odstępami nie mieszczą się w przeznaczonej na nie części panelu, lista akcji przewija się w swoim obrębie. Nie zabiera miejsca kontekstowi (pytaniu, rekomendacji) ani polom odpowiedzi.

---

## Acceptance Examples

- AE1. **Covers R2, R3, R4.** Na osi wybrany jest krok `implement`. `Enter` przenosi fokus do jego rozmowy, a `j` wybiera kolejny wpis, który dostaje kreskę cyan. Po `Esc` fokus wraca na oś, wciąż na `implement`, a kreska wpisu robi się szara. Drugi `Esc` wraca na listę zadań.
- AE2. **Covers R2.** Krok `analyze` ma stan „brak zapisu rozmowy”. `Enter` niczego nie zmienia i fokus zostaje na osi.
- AE3. **Covers R5.** Operator czyta rozmowę zakończonego kroku `plan`, gdy zadanie przechodzi do nowego kroku. Oś dalej pokazuje `plan` jako wybrany. Po powrocie na oś z włączonym śledzeniem zaznaczenie przechodzi na bieżący krok.
- AE4. **Covers R8, R10, R12.** Fokus jest na liście, wybrane jest zadanie „Refactor parser”. Nad nim i pod nim są grube cyan kreski poziome, a przy obu jego liniach jest gruba cyan kreska z lewej, która nie styka się z poziomymi. Po `Enter` fokus przechodzi do szczegółów, a klamra na liście zostaje, tyle że cienka i szara. Żaden wiersz listy się nie przesuwa.
- AE5. **Covers R13.** Zaznaczone zadanie z `!` i `✗` w czerwieni zachowuje czerwień markerów. Nic nie jest odwrócone.
- AE6. **Covers R14.** Z `NO_COLOR` zaznaczone zadanie na liście z fokusem ma grubą klamrę, a na liście bez fokusu cienką.
- AE7. **Covers R17.** Krok ma 6 akcji, a panel mieści 3 z odstępami. `j` przewija listę akcji tak, że zaznaczona akcja z klamrą jest widoczna, a pytanie i pole komentarza zostają na swoim miejscu.

---

## Success Criteria

- Operator czyta rozmowę kroku wpis po wpisie bez przechodzenia na zakładkę *Rozmowa*.
- TUI nie ma pełnych pasów odwróconych kolorów. Na pierwszy rzut oka widać, co jest wybrane i w którym panelu są klawisze.
- Plan nie musi wymyślać zachowania: poziomy fokusu, klawisze, kształt klamry, jej warianty i miejsca użycia są tu określone.

---

## Scope Boundaries

- Rozmowy i *Zdarzenia* nie dostają klamry ani odstępów. Na razie mają samą lewą kreskę.
- Bez zmian palety ról i zasady „tylko 16 kolorów ANSI”.
- Szerokość listy (28 kolumn) zostaje. Lista mieści mniej zadań i nie próbujemy tego kompensować.
- Inne wyróżnienia niż zaznaczenie (np. prezentacja błędu w transkrypcie, aktywna zakładka) nie są tu zmieniane.
- Bez nowych poziomów fokusu w innych zakładkach (np. *Dyrygent*: czat ↔ historia decyzji).

---

## Key Decisions

- Klamra zamiast samej kreski w listach: sygnał jest mocniejszy i odcina zaznaczony element od sąsiadów. Kosztem jest stały wiersz odstępu, przez co zadanie zajmuje 3 wiersze. Odstęp jest stały, żeby lista nie przeskakiwała przy ruchu kursora.
- Klamra bez narożników: operator chce, żeby lewa kreska była osobna od poziomych.
- Klamra bez fokusu jest cienka i szara, co rymuje się z ramkami paneli (`docs/brainstorms/2026-10-10-tui-czytelnosc-requirements.md`, R2). Zaznaczenie widać zawsze, a cyan zostaje tylko tam, gdzie są klawisze.
- Ta decyzja zastępuje R9 z dokumentu o czytelności TUI: zaznaczenie zadania nie jest już reverse video.
- Wejście w rozmowę kroku przez `Enter`/`→` i powrót przez `Esc`/`←` zamiast `Tab`. `Tab` jest zajęty przełączaniem treść ↔ panel odpowiedzi, a `Enter`/`Esc` to istniejący wzór lista ↔ szczegóły.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R10, R11][Technical] Jak pogodzić wiersz odstępu z minimalną wysokością terminala (60×20) i z wysokością panelu odpowiedzi. Ile akcji mieści się przy minimalnym rozmiarze?
- [Affects R2, R3][Technical] Czy `→`/`←` kolidują dziś z jakimś klawiszem w szczegółach (np. zmiana zakładki) i jak to rozstrzygnąć bez łamania zasady „jeden klawisz — jedno znaczenie”?
- [Affects R12, R16][Technical] Jakie znaki dają cienką i grubą wersję kresek (`─`/`━`, `│`/`┃`) i czy wyglądają spójnie z ramkami paneli w typowych fontach terminala.
