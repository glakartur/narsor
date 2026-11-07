# Subagenci i odmowy uprawnień — pomysły na przyszłość

Plan: `docs/plans/2026-11-07-1315-fix-subagenci-poza-glownym-watkiem-plan.md`

Dokument zbiera pomysły, które wyszły przy diagnozie zawieszenia taska
`task-326aba9d…` w autopilocie i które świadomie odroczono.

## 1. Flaga „odpowiedź końcowa jest poprawna" przy zawieszeniu na uprawnieniu

- [ ] **Gdy krok mimo wszystko zawiesza się na prośbie o uprawnienie, pokazać, czy tekst w `answer_tail` jest poprawną odpowiedzią końcową.**

**Stan obecny.** Zdarzenie `human_input_requested` z uprawnieniem niesie
`answer_tail`: końcowy tekst agenta. W zdarzeniu 224 był to kompletny, poprawny
wynik (`choice: findings`), ale operator widział tylko „needs a permission
decision…". Nie wiedział, że agent już skończył.

**Pomysł.** Silnik sprawdza `answer_tail` tymi samymi funkcjami co zwykła
ścieżka (`final_answer_kind`, `closing_answer`) i zapisuje wynik jako flagę
(np. `final_answer_valid`) w zdarzeniu. TUI pokazuje wiersz w rodzaju „agent
skończył poprawną odpowiedzią; odmowa: Bash `…`".

**Dlaczego odroczone.** Po zmianie z planu odmowa przy poprawnej odpowiedzi nie
zawiesza kroku. Flaga dotyczy tylko przypadków granicznych, które po naprawie
nie powinny się zdarzać. Wrócić do niej, jeśli takie zawieszenie się pojawi.

## 2. Dyrygent decyduje o uprawnieniach w autopilocie

- [ ] **Pytać dyrygenta o prośbę o uprawnienie, gdy autopilot jest włączony.**

**Stan obecny.** `answer_by_conductor` (`rescue.rs:390`) nie odpowiada na
prośby z `permission`. Operator zawsze dostaje okno uprawnień.

**Do rozstrzygnięcia.** Czy dyrygent może przyznawać uprawnienia (w
`conductor/invoke.rs` istnieje `permission_grants`). To zmiana projektu, nie
poprawka błędu.

## 3. Osobny, przełączany widok na wątek subagenta

- [ ] **Pozwolić przełączyć Rozmowę na wątek jednego subagenta.**

**Stan obecny (po planie).** Subagent to zwinięta grupa z nagłówkiem i wciętą
treścią w głównym wątku.

**Pomysł.** Wiersz z listą subagentów i klawisz zmieniający widok na jeden
wątek, podobnie jak pasek kroków w Rozmowie. Droższe niż grupa i niepotrzebne
do naprawy błędu.

## 4. Stan uruchomienia subagenta w nagłówku grupy

- [ ] **Pokazać w nagłówku grupy subagenta, czy ten subagent jeszcze działa.**

**Stan obecny.** Nagłówek grupy pokazuje nazwę, liczbę wpisów i liczbę błędów.
Nie pokazuje stanu, bo strumień Claude Code nie ma pewnego sygnału końca
subagenta. Subagent w tle odpowiada na wywołanie `Task` od razu i działa dalej,
więc wywnioskowany stan „zakończony” bywał fałszywy.

**Pomysł.** Adapter podaje jawny znak końca subagenta (jeśli strumień go niesie),
a widok pokazuje stan tekstem.
