---
date: 2026-09-26
topic: tui-nieblokujace-uruchomienia
---

# TUI: uruchomienia silnika bez blokowania interfejsu

## Summary

Cztery żądania TUI, które dziś wołają silnik synchronicznie (`StartNewTask`, `RunSelected`,
`SubmitInput`, `ApplyTransition`), zaczynają uruchamiać swoje wywołanie silnika w tle zamiast
wewnątrz jedynej gałęzi pętli zdarzeń, a wynik — łącznie z błędem infrastrukturalnym silnika, nie
tylko wynikiem workflow — wraca do interfejsu wyłącznie przez już istniejący strumień zdarzeń.

---

## Problem Frame

Dodanie nowego tasku (i wznowienie klawiszem `s`) zawiesza cały TUI po zatwierdzeniu formularza
Enterem: interfejs nie rysuje się na nowo i nie reaguje na żaden klawisz — nawet quit — aż do
zakończenia przetwarzania workflow do najbliższego stanu `human`. Przyczyna jest strukturalna, nie
punktowa: pętla zdarzeń w `src/tui/mod.rs:180` wywołuje `app.start_new_task().await` (i
analogicznie dla pozostałych trzech żądań) jako jedną gałąź `tokio::select!` — nic innego w tej
pętli (rysowanie, klawisze, strumień zdarzeń) nie jest odpytywane, dopóki ta gałąź się nie
zakończy. Dla domyślnego workflow `compound-engineering.yaml`, którego pierwszy stan to prawdziwe
wywołanie agenta, oznacza to zamrożenie na czas trwania realnego wywołania harnessu (i ewentualnie
`git clone` przy provisioning workspace'u).

Silnik ma już infrastrukturę, która nie jest tu wykorzystywana: `engine.subscribe()` i
`app.on_stream` już dziś zasilają widok na żywo bez blokowania, aktualizując dowolny wiersz listy
wykonań po `workflow_execution_id`, nie tylko wybrany. Osobny dokument,
`docs/brainstorms/2026-09-25-rozproszony-narsor-requirements.md`, opisuje docelowy kierunek, w
którym TUI przestaje w ogóle prowadzić pętlę silnika samodzielnie i rozmawia wyłącznie przez
zdarzenia z workerem/centralą (R10 tamtego dokumentu) — ta zmiana ma być krokiem w tamtą stronę,
nie konstrukcją do wyrzucenia przy późniejszej migracji.

---

## Actors

- A1. Operator: osoba obsługująca TUI, uruchamia i wznawia wykonania, odpowiada na pytania
  `human`, obserwuje listę wykonań.
- A2. Silnik (`Engine`): prowadzi pętlę kroków dla wykonania aż do granicy wymagającej człowieka,
  emitując zdarzenia po drodze; docelowo odpowiednik "workera" z dokumentu rozproszonego.

---

## Key Flows

- F1. Start nowego zadania, którego pierwszy krok jest długotrwały
  - **Trigger:** Operator zatwierdza formularz nowego zadania (Enter) dla workflow, którego
    pierwszy stan to akcja `agent`/`script`/`harness`.
  - **Actors:** A1, A2
  - **Steps:** Formularz tworzy wykonanie; prowadzenie silnika do najbliższego stanu `human`
    zostaje uruchomione w tle; TUI natychmiast wraca do rysowania i obsługi klawiszy; postęp
    (zmiana stanu, ewentualne niepowodzenie) dociera przez strumień zdarzeń i aktualizuje wiersz
    tego wykonania na liście; po zakończeniu kroku pojawia się notice z identyfikatorem
    wykonania.
  - **Outcome:** Interfejs pozostaje używalny przez cały czas trwania kroku; operator dowiaduje
    się o wyniku niezależnie od tego, co ma akurat zaznaczone.
  - **Covered by:** R1, R2, R4, R6

- F2. Dwa wykonania prowadzone jednocześnie
  - **Trigger:** Operator wznawia wykonanie B (`s`), podczas gdy wykonanie A nadal jest
    prowadzone w tle po wcześniejszym starcie.
  - **Actors:** A1, A2
  - **Steps:** Drugie prowadzenie rusza niezależnie od pierwszego; oba emitują zdarzenia do tego
    samego strumienia; lista aktualizuje status obu wierszy niezależnie.
  - **Outcome:** Operator nie musi czekać na zakończenie jednego wykonania, żeby pracować nad
    drugim.
  - **Covered by:** R3

- F3. Próba podwójnego prowadzenia tego samego wykonania
  - **Trigger:** Operator naciska `s` na wykonaniu, które jest już prowadzone w tle (np.
    przypadkowe podwójne naciśnięcie, albo wznowienie czegoś, co inny fragment UI już uruchomił).
  - **Actors:** A1, A2
  - **Steps:** System rozpoznaje, że `execution_id` jest już aktywnie prowadzone; druga próba
    zostaje odrzucona zamiast wystartować równoległego drugiego "prowadziciela" tego samego
    wykonania.
  - **Outcome:** Nigdy nie ma dwóch niezależnych prowadzeń tego samego wykonania naraz.
  - **Covered by:** R7

---

## Requirements

**Uruchamianie w tle**

- R1. Każde z czterech żądań TUI, które dziś wołają silnik synchronicznie wewnątrz pętli
  zdarzeń — `StartNewTask`, `RunSelected`, `SubmitInput`, `ApplyTransition` — uruchamia swoje
  wywołanie silnika (odpowiednio `engine.run`, `engine.resume`, `engine.deliver_human_input`)
  jako osobne zadanie w tle, a nie jako `.await` bezpośrednio w gałęzi `tokio::select!` pętli
  zdarzeń.
- R2. Pętla zdarzeń TUI (rysowanie, obsługa klawiszy, aktualizacja z eventów) pozostaje
  responsywna przez cały czas trwania kroku silnika w tle, niezależnie od czasu jego trwania.
- R3. Operator może mieć jednocześnie wiele wykonań prowadzonych w tle naraz, uruchomionych z
  osobnych naciśnięć `Enter`/`s`/odpowiedzi na formularz.

**Zdarzenia jako jedyny kanał wyniku**

- R4. Wynik prowadzenia w tle — zakończenie workflow, przejście w oczekiwanie na człowieka,
  niepowodzenie workflow, błąd infrastrukturalny silnika — dociera do TUI wyłącznie przez
  istniejący strumień zdarzeń (`engine.subscribe()` / `app.on_stream`); nie powstaje żaden nowy,
  osobny kanał wynikowy.
- R5. Błąd infrastrukturalny silnika (`EngineError`), dziś wyłącznie zwracany wywołującemu, staje
  się także zdarzeniem emitowanym przez silnik, tak by prowadzenie w tle mogło go zgłosić bez
  posiadania innego kanału zwrotnego niż strumień zdarzeń.
- R6. Gdy zdarzenie ze strumienia niesie zakończenie (sukces, porażka workflow, błąd
  infrastrukturalny) dla dowolnego wykonania na liście — nie tylko aktualnie wybranego — TUI
  ustawia widoczny na dole notice, nazwany identyfikatorem tego wykonania.

**Ochrona przed równoległym prowadzeniem tego samego wykonania**

- R7. System odmawia rozpoczęcia drugiego prowadzenia (`run`/`resume`/dostarczenia odpowiedzi) dla
  tego samego `execution_id`, dopóki poprzednie nie osiągnęło granicy kroku, i informuje o tym
  operatora notice'em zamiast cicho ignorować próbę albo dopuścić wyścig.

**Zasięg pauzy**

- R8. Istniejący mechanizm łagodnej pauzy (`pause_now` / `take_pause_request`, D34) działa bez
  zmian wobec wykonania prowadzonego w tle.
- R9. Natychmiastowe przerwanie akcji w locie (D33 kill procesu) pozostaje poza zakresem tej
  zmiany.

---

## Acceptance Examples

- AE1. **Covers R1, R2.** Operator zatwierdza formularz odpowiedzi na pytanie `human`, której
  wynikiem jest przejście do stanu z akcją `agent` trwającą kilkanaście sekund. W tym czasie
  operator porusza się po liście wykonań, otwiera i zamyka inny modal, i może nacisnąć `q` — TUI
  rysuje się i reaguje przez cały ten czas.
- AE2. **Covers R3.** Operator startuje zadanie A, którego pierwszy krok trwa kilka minut, i przed
  jego zakończeniem zaznacza inne, zapauzowane wykonanie B, po czym naciska `s`. Oba wykonania
  postępują niezależnie, a lista pokazuje zmieniający się status A i B osobno.
- AE3. **Covers R4, R5, R6.** Prowadzenie w tle wykonania X kończy się błędem infrastrukturalnym
  silnika (np. nie udało się zapisać stanu). Operator, który w międzyczasie zaznaczył inne
  wykonanie, widzi na dole notice nazwany identyfikatorem X i treścią błędu.
- AE4. **Covers R7.** Operator naciska `s` na wykonaniu Y, które jest już prowadzone w tle po
  wcześniejszym `s`. Druga próba zostaje odrzucona notice'em zamiast wystartować drugie,
  równoległe prowadzenie Y.

---

## Success Criteria

- Start lub wznowienie zadania, którego kolejny krok jest długotrwały (wywołanie agenta,
  `git clone`), nigdy nie zamraża rysowania ani obsługi klawiszy w TUI.
- Operator może poznać wynik prowadzenia w tle dla dowolnego wykonania na liście, nie tylko tego,
  które miał zaznaczone w chwili startu.
- Żadne dwa równoległe prowadzenia nie mogą naraz przesuwać tego samego `execution_id`.
- Istniejące zachowanie łagodnej pauzy działa bez zmian pod nowym modelem uruchamiania.
- Downstream (`ce-plan`) nie musi wymyślać, który z czterech żądań TUI jest objęty zmianą, ani
  jak wygląda zachowanie przy dwóch równoległych wykonaniach — obie rzeczy są tu rozstrzygnięte.

---

## Scope Boundaries

- Natychmiastowe przerwanie akcji w locie (D33 kill) — pozostaje osobnym, przyszłym tematem.
- Pełny podział na centralę i workery, dzierżawy, `epoch`, transport API/gniazdo unixowe,
  autoryzacja — to zakres dokumentu `2026-09-25-rozproszony-narsor-requirements.md`. Ta zmiana
  jest wyłącznie wewnątrzprocesowym krokiem w tamtą stronę, nie jego realizacją.
- Praca offline i wysoka dostępność centrali — nie dotyczy, bo centrala jeszcze nie istnieje.

---

## Key Decisions

- Wynik prowadzenia w tle wraca wyłącznie przez strumień zdarzeń, nie przez osobny kanał: to
  jedyny kanał, który przetrwa przyszłą migrację do modelu klient-API (R10 dokumentu
  rozproszonego) — dedykowany kanał wynikowy byłby konstrukcją do wyrzucenia.
- Dozwolone jest wiele równoległych wykonań z jednego TUI, nie jedno na raz: dopasowanie do
  przyszłego modelu wielu workerów, kosztem potrzeby strażnika przed podwójnym prowadzeniem tego
  samego `execution_id` (R7).
- W tym zakresie tylko łagodna pauza: natychmiastowe przerwanie procesu nie jest potrzebne do
  rozwiązania zgłoszonego zamrożenia UI i wymaga osobnego namysłu nad D33.

---

## Dependencies / Assumptions

- Zakłada się, że `Engine` (dzielony dziś jako `Arc<Engine>` między TUI a subskrypcję zdarzeń)
  jest bezpieczny do wywołań z wielu współbieżnych zadań jednocześnie.
- Zakłada się, że dodanie nowego wariantu reprezentującego błąd infrastrukturalny do zdarzeń
  emitowanych przez silnik nie łamie kompatybilności odczytu istniejącego logu zdarzeń dla
  wykonań już w toku — do zweryfikowania w planowaniu.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R5][Technical] Jak dokładnie reprezentować błąd infrastrukturalny silnika jako
  zdarzenie (nowy wariant `EventData` czy inny kształt) — zależne od istniejącej taksonomii
  zdarzeń w `src/domain/event.rs`.
- [Affects R1][Technical] Czy `App` musi przechowywać uchwyty (`JoinHandle`) zadań w tle dla
  potrzeb zamknięcia TUI (`q`), i jak ścieżka wyjścia z programu powinna zachować się wobec
  wciąż aktywnych prowadzeń.
- [Affects R7][Technical] Gdzie najlepiej umieścić strażnika "aktualnie prowadzone
  `execution_id`" — na `App`, na `Engine`, czy jako osobna struktura współdzielona przez oba.
