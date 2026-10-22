---
date: 2026-10-22
topic: tui-edycja-formularzy
---

# TUI: spójna edycja tekstu i nawigacja w formularzach

## Summary

Wszystkie formularze TUI dostają jedną, przewidywalną zasadę edycji i nawigacji: poprawny kursor, zawijanie linii do szerokości pola, pełne klawisze edycji (`Delete`, `Home`, `End`), wyraźne tło pola w edycji, przejście między polami strzałkami na brzegu pola, `Enter` jako „dalej" oraz popup potwierdzenia przed zatwierdzeniem.

---

## Problem Frame

Operator edytuje tekst w formularzach TUI (nowe zadanie, panel odpowiedzi, formularz błędu, zakładka Dyrygent, wybór repozytorium). Edycja zachowuje się nienaturalnie.

- Kursor jest wstawiany do tekstu jako osobny znak (`▏` w `src/tui/chooser.rs`). Po cofnięciu się o znak na ekranie widać `ab| c` zamiast `ab|c`: tekst za kursorem przesuwa się i wygląda na zmieniony.
- Długa linia nie jest łamana na szerokości pola. Dotyczy to pola edycji i zwykłego tekstu, np. opisu workflow w formularzu nowego zadania.
- Brak obsługi `Delete`, `Home` i `End`.
- Pole w edycji nie wyróżnia się od innych pól.
- Zasady przejścia między polami strzałkami są niespójne między formularzami.
- Formularz zatwierdza się od razu po `Enter` na ostatnim polu. Łatwo wysłać go przez pomyłkę.
- Pole edycji ma dodatkowe znaki wizualne i nie jest dopasowane do szerokości formularza.

---

## Actors

- A1. Operator: pisze i poprawia tekst w formularzach, przechodzi między polami klawiaturą, zatwierdza formularz.

---

## Key Flows

- F1. Wypełnienie i zatwierdzenie formularza
  - **Trigger:** Operator otwiera dowolny formularz TUI.
  - **Actors:** A1
  - **Steps:** Focus jest na pierwszym polu od góry. Operator wpisuje tekst i przechodzi `Enter` do następnych pól. `Enter` na ostatnim polu otwiera popup potwierdzenia. `Enter` w popupie zatwierdza formularz.
  - **Outcome:** Formularz został wysłany dokładnie raz, po jawnym potwierdzeniu.
  - **Covered by:** R9, R10, R11, R12, R13

- F2. Powrót z popupu do edycji
  - **Trigger:** Popup potwierdzenia jest otwarty.
  - **Actors:** A1
  - **Steps:** Operator naciska `Esc`. Popup się zamyka. Focus wraca na pole, które miało focus przed `Enter`. Wpisane wartości zostają.
  - **Outcome:** Operator kontynuuje edycję bez utraty danych.
  - **Covered by:** R12, R14

---

## Requirements

**Edycja tekstu**
- R1. Kursor nie zmienia położenia ani wyglądu tekstu. Znaki przed i za kursorem stoją w tych samych komórkach niezależnie od pozycji kursora (nie ma dodatkowej przerwy ani przesunięcia).
- R2. `Delete` usuwa znak za kursorem. Na końcu tekstu nic nie robi.
- R3. `Home` przenosi kursor na początek bieżącej linii. `End` przenosi kursor na koniec bieżącej linii. „Linia" oznacza linię widoczną na ekranie po zawinięciu (patrz R5). Decyzja w „Założenia do potwierdzenia" (Z2).
- R4. Reguły R1–R3 działają w każdym polu tekstowym każdego formularza TUI, tak jak dziś działa wspólny kod edycji.

**Zawijanie i układ pola**
- R5. Tekst w polu edycji zawija się na szerokości pola. Kursor i przewijanie pola nadal wskazują właściwy znak w zawiniętym tekście.
- R6. Tekst wyświetlany bez edycji w formularzach (np. opis workflow w formularzu nowego zadania, opisy pól) też zawija się do dostępnej szerokości. Nie jest ucinany ani wychodzi poza ramkę.
- R7. Pole edycji zaczyna się po znaku `>`, czyli jest wcięte o 1 znak względem formularza. Kończy się 1 znak przed końcem szerokości formularza. Za polem nie ma dodatkowych znaków wizualnych. Szerokość zawijania z R5 wynika z tego zakresu.
- R8. Pole, które jest w danym momencie w edycji, ma delikatnie zmienione tło (granat). Pozostałe pola mają tło standardowe. Kolor pochodzi ze wspólnego stylu TUI (`src/tui/style.rs`) i jest czytelny w motywach jasnym i ciemnym. Decyzja w Z3.

**Nawigacja między polami**
- R9. Focus początkowy w każdym formularzu jest na pierwszym polu od góry. Dotyczy to też formularzy, które dziś otwierają się na innej pozycji (np. formularz z listą wyboru).
- R10. Strzałka w górę na pierwszej linii pola przenosi na pole wyżej. Strzałka w dół na ostatniej linii pola przenosi na pole następne. Przejście następuje dopiero przy powtórzeniu strzałki: pierwsze naciśnięcie na brzegu pola tylko ustawia kursor na początku (w górę) lub końcu (w dół) tekstu, kolejne to samo naciśnięcie przechodzi do sąsiedniego pola. Dotyczy to każdego formularza i pól wielolinijkowych, jednolinijkowych oraz wyborów.
- R11. `Enter` przechodzi do następnego pola. `Enter` na ostatnim polu zatwierdza formularz (zasada bez zmian), ale z potwierdzeniem z R12. Nowa linia w polach wielolinijkowych nadal działa przez `Ctrl+J` (i `Shift+Enter`/`Alt+Enter` tam, gdzie terminal je rozróżnia).

**Potwierdzenie formularza**
- R12. `Enter` na ostatnim polu każdego formularza otwiera popup z pytaniem o potwierdzenie zatwierdzenia. Dotyczy to formularzy, które dziś wysyłają dane: nowe zadanie, panel odpowiedzi (każdy rodzaj), formularz błędu, zakładka Dyrygent. Nie dotyczy zwykłych list wyboru bez pól tekstowych (Z4).
- R13. W popupie `Enter` zatwierdza formularz. Inne klawisze (poza `Esc`) niczego nie zatwierdzają.
- R14. W popupie `Esc` zamyka popup i wraca do edycji na polu, które miało focus przed `Enter`. Wartości i kursor zostają.
- R15. Popup i pasek skrótów trzymają się kontraktu klawiatury modalnych potwierdzeń (`docs/solutions/design-patterns/tui-modal-confirmation-keyboard-contract.md`): podczas popupu działają tylko jego klawisze, pasek skrótów pokazuje tylko je, popup jest widoczny także w małym terminalu.

---

## Acceptance Examples

- AE1. **Covers R1.** Given pole z tekstem `abc` i kursorem na końcu, when operator naciska `←`, then na ekranie widać `ab` i `c` obok siebie z kursorem między nimi, a `c` nie zmienia pozycji.
- AE2. **Covers R2.** Given tekst `abc` z kursorem po `a`, when operator naciska `Delete`, then tekst to `ac` z kursorem po `a`. Given kursor na końcu, `Delete` nie zmienia tekstu.
- AE3. **Covers R3, R5.** Given długi tekst zawinięty na trzy linie widoczne i kursor w środku drugiej, when operator naciska `Home`, then kursor jest na początku drugiej widocznej linii. `End` ustawia go na końcu tej linii.
- AE4. **Covers R6.** Given workflow z opisem dłuższym niż szerokość panelu w formularzu nowego zadania, when formularz się rysuje, then opis zawija się do szerokości panelu w całości.
- AE5. **Covers R7.** Given formularz o szerokości 40 komórek, when pole edycji jest narysowane, then `>` stoi w kolumnie wcięcia, a tekst pola kończy się najpóźniej w komórce 39 bez znaku za polem.
- AE6. **Covers R10.** Given kursor na pierwszej linii pola, nie na początku tekstu, when operator naciska `↑`, then kursor idzie na początek tekstu. Gdy naciśnie `↑` ponownie, focus przechodzi na pole wyżej. Symetrycznie `↓` na ostatniej linii.
- AE7. **Covers R9.** Given dowolny formularz, when się otwiera, then focus jest na pierwszym polu od góry.
- AE8. **Covers R12, R13, R14.** Given focus na ostatnim polu, when operator naciska `Enter`, then pojawia się popup. `Esc` wraca na to samo pole z zachowanym tekstem. Ponowne `Enter` na polu i `Enter` w popupie wysyła formularz raz.

---

## Success Criteria

- Operator edytuje tekst w każdym formularzu bez niespodzianek: kursor stoi tam, gdzie go widać, a `Delete`, `Home` i `End` robią to, co w typowym edytorze.
- Żaden formularz z polami tekstowymi nie zostaje wysłany jednym `Enter` bez potwierdzenia.
- Planowanie nie musi zgadywać zasad nawigacji, układu pola ani zachowania popupu: są ustalone tutaj.

---

## Scope Boundaries

- Nie zmieniamy treści formularzy, pól ani walidacji.
- Nie dodajemy zaznaczania tekstu, kopiowania, cofania (`undo`) ani skoku po słowach (`Ctrl+←/→`).
- Nie zmieniamy obsługi wklejania wieloliniowego ani klawiszy nowej linii.
- Nie zmieniamy ramek, kolorów poza tłem pola w edycji i układu ekranów poza polami formularzy.
- Nie obejmuje edycji tekstu poza formularzami (np. pole rozmowy z agentem w widoku zadania), chyba że używa tego samego wspólnego pola — wtedy dostaje R1–R5 automatycznie, a R10–R14 go nie dotyczą (Z4).

---

## Key Decisions

- Jedna zasada dla wszystkich formularzy: zmiana we wspólnym kodzie pola i formularza, nie osobne poprawki per formularz.
- Przejście strzałką dopiero przy powtórzeniu: zapobiega przypadkowej ucieczce z pola przy pisaniu wielu linii.
- Potwierdzenie ma jeden klawisz zatwierdzający (`Enter`) i jeden wycofujący (`Esc`), jak w istniejących modalach.

---

## Dependencies / Assumptions

- Formularze współdzielą wspólny kod pola i formularza (`src/tui/chooser.rs`, `src/tui/input_form.rs`). Zweryfikowano w kodzie: pole tekstowe, edycja, kursor i rysowanie pól są w jednym miejscu. Nie sprawdzono jeszcze, czy zakładka Dyrygent i formularz błędu używają ich bez wyjątków.
- Terminal przekazuje `Home`, `End` i `Delete` jako osobne klawisze. Nie zweryfikowano.

---

## Założenia do potwierdzenia

- Z1. Przyczyna błędu kursora to wstawianie znaku kursora do tekstu (`▏`, szerokość w terminalu może być niejednoznaczna). Wymaganie opisuje skutek (R1), nie sposób naprawy. Przyczynę potwierdza planowanie.
- Z2. `Home`/`End` działają na linii widocznej po zawinięciu, nie na całej linii logicznej (do znaku `\n`). Taki wybór zgadza się z oczekiwaniem „początek i koniec bieżącej linii".
- Z3. „Granat" oznacza ciemne, stonowane tło pola w edycji. Dokładny odcień wybiera planowanie ze stylów TUI. Bez zmiany koloru tekstu.
- Z4. Popup potwierdzenia dotyczy formularzy z polami, które wysyłają dane (R12). Wybór z samej listy (bez pól) nadal działa jednym `Enter`. Panel odpowiedzi z wyborem „zatwierdź/odrzuć" bez pól tekstowych też zostaje bez popupu.
- Z5. Powtórzenie strzałki (R10) liczy się jako dwa kolejne naciśnięcia tej samej strzałki bez innego klawisza pomiędzy. Pierwsze naciśnięcie na brzegu pola, gdy kursor już jest na początku/końcu tekstu, liczy się jako pierwsze z dwóch (więc przejście wymaga jeszcze jednego). Decyzja, czy kursor „już na brzegu" przechodzi od razu, jest w pytaniach otwartych.
- Z6. Pola typu wybór (`‹ … ›`) i liczba mają ten sam układ i tło w edycji co pola tekstowe. Strzałki `←`/`→` zmieniają wartość wyboru, jak dziś.
- Z7. Popup potwierdzenia ma stały tekst ogólny (np. „Zatwierdzić formularz?") z podpowiedzią klawiszy. Wartości pól nie są w nim powtarzane.

---

## Pytania otwarte dla użytkownika

- Czy przy powtórzeniu strzałki (R10) pierwsze naciśnięcie na brzegu pola ma zawsze tylko ustawić kursor na początku/końcu tekstu, czy gdy kursor już jest na początku (w górę) lub na końcu (w dół), przejście ma nastąpić od razu? Założono wersję ostrożną: dwa naciśnięcia, zawsze.
- Czy popup potwierdzenia ma pokazywać krótkie podsumowanie danych (np. tytuł zadania), czy wystarcza ogólne pytanie (Z7)?
- Czy wybór z listy w panelu odpowiedzi (bez pól tekstowych) też ma wymagać potwierdzenia? Założono, że nie (Z4).

---

## Outstanding Questions

### Resolve Before Planning

- Brak: założenia i pytania powyżej nie blokują planowania.

### Deferred to Planning

- [Affects R1][Technical] Jak narysować kursor bez wstawiania znaku do tekstu (styl komórki pod kursorem lub kursor terminala).
- [Affects R5, R6][Technical] Jak połączyć zawijanie z wyliczaniem pozycji kursora, przewijaniem i `Up`/`Down` po liniach widocznych.
- [Affects R8][Needs research] Który odcień granatu jest czytelny w obu motywach i spójny z istniejącymi rolami stylu.
- [Affects R12][Technical] Czy popup współdzielić z istniejącym kontraktem modalnym, a które formularze dziś omijają wspólny kod.
