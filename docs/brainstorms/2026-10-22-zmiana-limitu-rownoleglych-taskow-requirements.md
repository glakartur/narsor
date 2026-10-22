---
date: 2026-10-22
topic: zmiana-limitu-rownoleglych-taskow
---

# Zmiana limitu równolegle przetwarzanych tasków w locie

## Summary

Operator może zmienić limit równoległych tasków backendu na dwa sposoby: trwale w pliku konfiguracyjnym backendu oraz ad hoc w działającej centrali, bez restartu. Podniesienie limitu uruchamia kolejne taski z kolejki od razu. Obniżenie limitu poniżej liczby działających tasków nie zabija żadnego z nich: centrala czeka, aż pierwszy task dojdzie do bezpiecznego punktu, i dopiero wtedy go zatrzymuje, aż liczba tasków spadnie do nowego limitu.

---

## Problem Frame

Dziś każdy backend ma twardy limit slotów (`max_concurrent_tasks` w pliku backendu, zakres 1–1024; D62). Centrala czyta go na świeżo przy każdej decyzji. Operator, który chce przetwarzać więcej tasków naraz, musi edytować plik na dysku. Nie ma polecenia ani widoku, który zmienia limit w działającej centrali i pokazuje skutek.

Obniżenie limitu jest dziś bezpieczne, ale bierne. Centrala nigdy nie odbiera slotu działającemu taskowi (R11 schedulera). Po obniżeniu limitu liczba działających tasków pozostaje wyższa od limitu, aż któryś task skończy się sam. Operator nie ma jak przyspieszyć tego w kontrolowany sposób, np. gdy maszyna jest przeciążona. Nie widzi też, że limit „czeka" na wykonanie.

Dotyczy to operatora, który prowadzi kilka tasków na jednym backendzie (lokalnym lub zdalnym) i zmienia swoje zapotrzebowanie w trakcie dnia.

---

## Actors

- A1. Operator: zmienia limit w pliku konfiguracyjnym lub w działającej centrali i obserwuje skutek.
- A2. Centrala (control plane): decyduje o slotach, uruchamia taski z kolejki, zatrzymuje nadwyżkę tasków w bezpiecznym punkcie.
- A3. Runner taska: dochodzi do bezpiecznego punktu i zatrzymuje się na polecenie centrali.

---

## Key Flows

- F1. Podniesienie limitu
  - **Trigger:** Operator podnosi limit (plik albo zmiana ad hoc).
  - **Actors:** A1, A2
  - **Steps:** Centrala przyjmuje nową wartość. Centrala liczy wolne sloty. Centrala uruchamia czekające taski z kolejki w ich kolejności, aż sloty się zapełnią.
  - **Outcome:** Dodatkowe taski ruszają bez restartu centrali i bez wpływu na taski, które już działają.
  - **Covered by:** R1, R2, R3, R6

- F2. Obniżenie limitu poniżej liczby działających tasków
  - **Trigger:** Operator obniża limit do wartości mniejszej niż liczba tasków, które trzymają slot.
  - **Actors:** A1, A2, A3
  - **Steps:** Centrala przyjmuje nowy limit i przestaje uruchamiać nowe taski. Centrala liczy nadwyżkę (trzymane sloty minus limit). Dla każdego tasku w stanie, który da się zatrzymać, centrala czeka na bezpieczny punkt. Gdy pierwszy task go osiągnie, centrala zatrzymuje go i zwalnia slot. Kroki się powtarzają, aż nadwyżka wyniesie zero. Operator widzi w tym czasie, że limit „czeka na zbieżność".
  - **Outcome:** Liczba tasków trzymających slot spada do nowego limitu. Żaden task nie traci pracy z kroku w toku.
  - **Covered by:** R4, R5, R7, R8, R9, R10

- F3. Wycofanie zmiany ad hoc
  - **Trigger:** Operator usuwa nadpisanie ad hoc.
  - **Actors:** A1, A2
  - **Steps:** Centrala wraca do wartości z pliku konfiguracyjnego. Dalej działa F1 albo F2, zależnie od różnicy.
  - **Outcome:** Obowiązuje jedna, jednoznaczna wartość.
  - **Covered by:** R2, R3

---

## Requirements

**Źródła limitu**
- R1. Limit można ustawić w pliku konfiguracyjnym backendu. Zmiana pliku działa bez restartu centrali (tak jak dziś) i uruchamia zarówno F1, jak i F2.
- R2. Operator może zmienić limit ad hoc w działającej centrali, osobno dla każdego backendu. Zmiana ad hoc ma pierwszeństwo przed plikiem i nie zmienia pliku.
- R3. Operator widzi aktualnie obowiązujący limit, jego źródło (plik albo ad hoc) i wartość z pliku. Operator może usunąć nadpisanie ad hoc i wrócić do wartości z pliku.
- R4. Nowa wartość podlega tej samej walidacji co dziś (liczba całkowita od 1 do 1024). Błędna wartość jest odrzucona z czytelnym komunikatem i nie zmienia obowiązującego limitu.

**Podniesienie limitu**
- R5. Po podniesieniu limitu centrala uruchamia czekające taski w kolejności kolejki, bez czekania na inne zdarzenie, np. na zakończenie innego taska.
- R6. Podniesienie limitu nigdy nie zatrzymuje ani nie zmienia tasków, które już działają.

**Obniżenie limitu**
- R7. Obniżenie limitu nie zatrzymuje tasków natychmiast. Centrala natychmiast przestaje uruchamiać nowe taski ponad nowy limit.
- R8. Gdy liczba tasków trzymających slot jest większa od limitu, centrala zatrzymuje nadwyżkę po jednym tasku. Zatrzymanie następuje dopiero, gdy task jest w bezpiecznym punkcie (ta sama ścieżka bezpiecznego zatrzymania, której używa dziś zwolnienie slotu z polityki). Centrala wybiera pierwszy task, który ten punkt osiągnie. Centrala nie wybiera tasków z góry.
- R9. Zatrzymany w ten sposób task zachowuje cały stan pracy. Task nie jest oznaczony jako zatrzymany przez właściciela. Task wraca do kolejki przed wszystkimi oczekującymi taskami (tak jak wznowienie zatrzymanego taska przy pełnym backendzie w D62) i rusza ponownie, gdy slot się zwolni.
- R10. Task, który czeka na odpowiedź właściciela albo ma nierozpatrzone polecenie operatora, jest pomijany przy wyborze nadwyżki, dopóki ta praca czeka. Centrala nie zatrzymuje taska w środku kroku.
- R11. Task, który z innego powodu zwalnia slot (zakończenie, anulowanie, zwolnienie z polityki w D62), liczy się do spadku nadwyżki. Centrala nie zatrzymuje wtedy dodatkowych tasków ponad potrzebę.
- R12. Gdy nadwyżka istnieje, operator widzi to w liście tasków i w widoku limitu: ile slotów jest trzymanych, jaki jest limit, ile tasków czeka na zatrzymanie. Zdarzenia zatrzymania z powodu obniżenia limitu zostają w historii taska z powodem.
- R13. Operator może zmienić limit ad hoc z TUI i z CLI. Obie powierzchnie pokazują ten sam stan, który podaje centrala. Klienci niczego sami nie wyprowadzają (zasada D62).

**Trwałość i bezpieczeństwo**
- R14. Zmiana ad hoc przeżywa restart centrali. Operator widzi, że nadpisanie jest aktywne.
- R15. Dwie jednoczesne zmiany limitu nie dają niespójnego stanu: obowiązuje ostatnia zatwierdzona, a limit i liczenie nadwyżki nigdy nie dopuszczają uruchomienia taska ponad limit.

---

## Acceptance Examples

- AE1. **Covers R5, R6.** Given backend z limitem 2, dwoma działającymi taskami i trzema czekającymi w kolejce, gdy operator ustawia limit 4, dwa pierwsze czekające taski ruszają w kolejności kolejki. Dwa działające taski działają dalej bez zmian. Trzeci czekający zostaje w kolejce.
- AE2. **Covers R7, R8, R9.** Given limit 3 i trzy działające taski, w tym task A w środku kroku, gdy operator obniża limit do 2, a task B pierwszy dochodzi do bezpiecznego punktu, centrala zatrzymuje B i zwalnia jego slot. Task A działa dalej do końca kroku. B wraca do kolejki przed oczekującymi, ze stanem pracy bez zmian.
- AE3. **Covers R7.** Given limit 3 i trzy działające taski, gdy operator obniża limit do 2 i task kończy się sam, centrala nie zatrzymuje żadnego innego taska.
- AE4. **Covers R10.** Given nadwyżka 1 i jedyny kandydat czeka na odpowiedź właściciela, gdy odpowiedź jest nierozpatrzona, centrala nie zatrzymuje tego taska. Operator widzi, że nadwyżka czeka.
- AE5. **Covers R2, R3.** Given limit w pliku 3 i nadpisanie ad hoc 5, gdy operator usuwa nadpisanie, obowiązuje limit 3 i działa F2, jeśli trzymanych slotów jest więcej niż 3.
- AE6. **Covers R4.** Given limit 3, gdy operator wpisuje 0 albo 2000, centrala odrzuca wartość z komunikatem. Limit zostaje 3.

---

## Success Criteria

- Operator zmienia liczbę równoległych tasków w kilka sekund, z TUI lub CLI, bez edycji pliku i bez restartu centrali.
- Po obniżeniu limitu operator nie traci pracy z żadnego kroku w toku. Liczba tasków trzymających slot zbiega do nowego limitu bez ręcznej interwencji.
- Planowanie nie musi wymyślać reguł wyboru taska do zatrzymania ani zachowania zatrzymanego taska.

---

## Scope Boundaries

- Brak automatycznego dobierania limitu (np. wg obciążenia maszyny).
- Brak natychmiastowego, wymuszonego zatrzymania tasków przy obniżeniu limitu.
- Brak wyboru przez operatora, który task zatrzymać. Zatrzymuje się pierwszy gotowy.
- Brak zmian w kolejności kolejki, priorytetu i w zasadach zwalniania slotów z D62 poza opisanymi powyżej.
- Brak limitu globalnego ponad backendami. Limit pozostaje ustawieniem backendu.
- Brak zmiany zakresu 1–1024.

---

## Key Decisions

- **Bezpieczny punkt to istniejąca ścieżka zatrzymania z polityki (D62), nie nowy mechanizm.** Dzięki temu zatrzymanie nadwyżki zachowuje te same gwarancje co zwolnienie slotu.
- **Zatrzymuje się pierwszy task, który jest gotowy, a nie task wybrany z góry.** Odpowiada to intencji z opisu: centrala czeka na pierwszą sesję w bezpiecznym stanie.
- **Zmiana ad hoc nie zapisuje pliku.** Plik pozostaje deklaracją operatora. Nadpisanie jest osobnym, widocznym stanem.

---

## Dependencies / Assumptions

- Dziś limit jest ustawieniem backendu (`max_concurrent_tasks` w `limits`, `crates/narsor-engine/src/environment/backend.rs`). Centrala czyta go na świeżo przy każdej decyzji.
- Dziś scheduler nigdy nie zabiera slotu działającemu taskowi (`crates/narsor-engine/src/orchestration/scheduler.rs`, zasada R11 „nothing running is ever preempted"). Ta funkcja zmienia tę zasadę wyłącznie dla nadwyżki po obniżeniu limitu i tylko w bezpiecznym punkcie. Ten wyjątek wymaga zapisu w `ARCHITECTURE_DECISIONS.md`.
- Istnieje ścieżka bezpiecznego zatrzymania z aktorem `Policy` (D62). Czy dokładnie spełnia pojęcie „bezpiecznego punktu" tutaj, sprawdza planowanie.

---

## Założenia do potwierdzenia

- Z1. „Koordynator tasków" w opisie to centrala (control plane) wraz z daemonem. „Aktywna sesja koordynatora" to działająca centrala. Zmiana ad hoc dotyczy działającego procesu.
- Z2. Limit pozostaje ustawieniem każdego backendu osobno. Konfiguracja „w pliku" to plik backendu, a nie nowy, globalny plik.
- Z3. „Bezpieczny punkt" to ten sam punkt, w którym dziś zatrzymuje się runner przy `Pause` z polityki (po zakończeniu bieżącego kroku lub w innym punkcie, który ta ścieżka uznaje za bezpieczny). Zakładamy, że to odpowiada opisowi „po zakończeniu jakiegoś kroku". Potwierdza to użytkownik i weryfikuje specjalista, bo punkt zależy od tego, jak runner obsługuje pauzę.
- Z4. Zatrzymany z powodu limitu task wraca do kolejki na początek (jak w D62), a nie na koniec.
- Z5. Zmiana ad hoc przeżywa restart centrali (R14). Bezpieczniejsze dla operatora niż ciche wycofanie.
- Z6. Zmiana ad hoc jest dostępna dla właściciela centrali, tak jak inne polecenia sterujące. Osobny model uprawnień nie jest potrzebny.

---

## Pytania otwarte dla użytkownika

- P1. Czy zmiana ad hoc ma przeżywać restart centrali (Z5), czy wygasać przy restarcie i wracać do pliku?
- P2. Czy zatrzymany task ma wracać na początek kolejki (Z4), czy na koniec? Początek chroni task przed głodzeniem. Koniec daje szansę taskom, które czekały dłużej.
- P3. Czy limit ma być także ustawiany w pliku ogólnych ustawień (np. jedna wartość dla backendu `local`), czy wystarczy plik backendu?
- P4. Czy zatrzymanie nadwyżki ma dotyczyć też tasków `Interrupted` i `Unknown`, które trzymają slot, a nie mają działającego runnera? Zakładamy, że nie, bo nie ma czego zatrzymać w bezpiecznym punkcie.
- P5. Czy operator chce widzieć, który task jest następny w kolejce do zatrzymania, czy wystarczy licznik nadwyżki?

---

## Outstanding Questions

### Deferred to Planning

- [Affects R8, R10][Technical] Jak centrala rozpoznaje „task dotarł do bezpiecznego punktu" i jak zapobiega wyścigowi z poleceniem właściciela.
- [Affects R14][Technical] Gdzie zapisać nadpisanie ad hoc, żeby przeżyło restart i nie kolidowało z odczytem pliku.
- [Affects R13][Technical] Jakie polecenie CLI i jaki widok TUI wystawiają zmianę.
- [Affects R5][Technical] Co wyzwala ponowne planowanie po zmianie limitu (pętla co 5 s czy zdarzenie).
