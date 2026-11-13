---
date: 2026-11-13
topic: obserwatory-zrodel-zewnetrznych
---

# Obserwatory źródeł zewnętrznych: Jira → nowe taski, PR/MR → zdarzenia w taskach

## Summary

Narsor dostaje „obserwatory". Obserwator to nazwana automatyzacja. Ma skrypt sprawdzający, harmonogram, warunek ustawiony przez użytkownika i skutek. Są dwa skutki: utworzenie nowego taska (na przykład z zadania w Jirze) albo przekazanie zdarzenia do istniejącego taska (na przykład koniec code review na PR w GitHub lub MR w GitLab). Silnik uruchamia skrypty shellowe. Agent pomaga napisać skrypt, a człowiek zatwierdza skrypt przed pierwszym uruchomieniem. Obserwator PR działa tylko wtedy, gdy ma co najmniej jeden wskazany PR/MR.

---

## Problem Frame

Dziś Narsor reaguje tylko na polecenia operatora: `narsor-cli task create`, `task input`, akcje w TUI. Świat zewnętrzny zmienia się bez wiedzy Narsora:

- Zadanie w Jirze dostaje etykietę lub status, który oznacza „do zrobienia przez fabrykę". Operator musi to zobaczyć i ręcznie utworzyć task.
- Recenzent kończy code review na PR/MR. Task, który czeka na review, nie wie o tym. Operator musi przenieść informację ręcznie przez `task input`.

Każde takie przeniesienie kosztuje czas operatora i daje opóźnienie. Operator może też pominąć zdarzenie. Integracje z Jirą, GitHub i GitLab różnią się między zespołami (pola, statusy, JQL, etykiety, typy zdarzeń review). Jedna sztywna integracja wbudowana w kod nie pokryje tych różnic.

Stan w repozytorium (zweryfikowany):

- Model domeny ma typ akcji `WaitAction` („czekanie na zdarzenie zewnętrzne") z polami `event`, `filter`, `timeout_secs` (`crates/narsor-engine/src/domain/workflow_definition.rs`). Akcja nie ma wykonawcy (`crates/narsor-engine/src/engine/runner/step/action_kind.rs`).
- Protokół ma zdarzenie `ExternalEventReceived { event_type, source, payload }` (`crates/narsor-protocol/src/event.rs`). Nic go dziś nie wysyła.
- Control plane ma rejestr sekretów backendu (`docs/operations/remote-backends.md`, sekcja Secrets).
- Nie znaleziono w repozytorium harmonogramu zadań cyklicznych ani pojęcia obserwatora (wyszukiwanie: `cron`, `schedule`, `watch`, `monitor` w dokumentacji; niezweryfikowane w całym kodzie).

---

## Actors

- A1. Twórca automatyzacji: osoba, która definiuje obserwator, jego warunek i skutek. W trybie sieciowym jest to właściciel instalacji.
- A2. Agent: pomaga napisać skrypt obserwatora z opisu warunku w języku naturalnym.
- A3. Silnik obserwatorów: uruchamia skrypty według harmonogramu, pilnuje limitów, zapisuje wynik każdego uruchomienia.
- A4. Control plane (`narsor-server`): tworzy taski i dostarcza zdarzenia do tasków.
- A5. Task (workflow): odbiera zdarzenie i może na nie czekać w kroku `wait`.
- A6. Źródło zewnętrzne: Jira, GitHub, GitLab.

---

## Key Flows

- F1. Utworzenie obserwatora z pomocą agenta
  - **Trigger:** A1 opisuje warunek, na przykład „zadania z projektu ABC z etykietą `narsor` w statusie To Do".
  - **Actors:** A1, A2, A3
  - **Steps:** A1 podaje źródło, warunek, skutek i harmonogram. A2 pisze skrypt. A3 wykonuje przebieg próbny: skrypt działa, ale skutek nie występuje. A1 widzi listę wykrytych zmian. A1 zatwierdza skrypt albo prosi agenta o poprawkę.
  - **Outcome:** Obserwator jest aktywny. Zatwierdzona wersja skryptu jest zapisana.
  - **Escape:** A1 odrzuca skrypt. Obserwator nie jest aktywny.
  - **Covered by:** R1, R2, R9, R10, R11

- F2. Jira → nowy task
  - **Trigger:** Harmonogram obserwatora Jira.
  - **Actors:** A3, A4, A6
  - **Steps:** A3 uruchamia skrypt. Skrypt znajduje nowe zadania, które spełniają warunek, i sam wywołuje `narsor-cli task create` (z jednego zadania może utworzyć wiele tasków). Skrypt przekazuje klucz skutku. A4 pomija skutek, którego klucz już wykonał.
  - **Outcome:** Jedno zadanie w Jirze daje dokładnie jeden task.
  - **Covered by:** R3, R12, R13, R14, R15

- F3. Koniec review na PR/MR → zdarzenie w tasku
  - **Trigger:** Task lub A1 wskazuje PR/MR do obserwacji i typy zdarzeń, na przykład „review zatwierdzone" albo „zmiany wymagane".
  - **Actors:** A1, A3, A4, A5, A6
  - **Steps:** A3 uruchamia skrypt tylko dla wskazanych PR/MR. Skrypt sam wywołuje `narsor-cli task input` dla zdarzeń wybranego typu. A4 zapisuje zdarzenie w historii taska powiązanego z danym PR/MR. Task, który w chwili zdarzenia czeka w kroku `wait` z pasującym zdarzeniem, idzie dalej.
  - **Outcome:** Task wie o końcu review bez udziału operatora.
  - **Escape:** PR/MR jest zamknięty lub scalony, albo task jest zarchiwizowany. Obserwacja tego PR/MR kończy się.
  - **Covered by:** R4, R5, R6, R7, R8, R16, R17

---

## Requirements

**Model obserwatora**
- R1. Obserwator ma: nazwę, źródło, skrypt, warunek (parametry przekazane do skryptu), harmonogram, skutek i stan (aktywny, wstrzymany, w błędzie).
- R2. Skrypt jest skryptem shellowym. Narsor nie ma wbudowanych integracji z Jirą, GitHub ani GitLab. Narsor może dostarczyć przykładowe skrypty jako punkt startowy.
- R3. Skutek „utwórz task": obserwator wskazuje workflow i mapowanie danych zdarzenia na wejście workflow (na przykład klucz zadania → `ticket`).
- R4. Skutek „przekaż zdarzenie": skrypt dostarcza zdarzenie do taska powiązanego z obserwowanym obiektem (PR/MR). Zdarzenie ma typ, źródło i dane. Narsor zapisuje zdarzenie w historii taska. Krok `wait` widzi tylko zdarzenia, które nadeszły w czasie czekania. Zdarzenie dla taska, który nie czeka, nie zmienia stanu taska.

**Obserwacja wskazanych obiektów (PR/MR)**
- R5. Obserwator PR/MR obserwuje tylko obiekty z jawnej listy. Każdy wpis listy wskazuje PR/MR, task-adresata i typy zdarzeń do wyłapania.
- R6. Gdy lista jest pusta, silnik nie uruchamia skryptu.
- R7. Wpis znika z listy, gdy PR/MR jest zamknięty lub scalony, gdy task-adresat jest zakończony lub zarchiwizowany, albo gdy A1 usunie wpis.
- R8. Wpis do listy dodaje krok workflow (jawna deklaracja w YAML, na przykład krok, który otworzył PR i czeka na review) albo operator ręcznie przez CLI. Narsor nie dodaje wpisów automatycznie.

**Tworzenie skryptu z agentem**
- R9. A1 może poprosić agenta o skrypt na podstawie opisu warunku. Agent zna kontrakt skryptu (R12) i przykładowe skrypty.
- R10. Przebieg próbny uruchamia skrypt i pokazuje wykryte zmiany. W przebiegu próbnym `narsor-cli` nie tworzy tasków i nie dostarcza zdarzeń, tylko zapisuje, co zrobiłby.
- R11. Nowy lub zmieniony skrypt nie działa według harmonogramu, dopóki człowiek go nie zatwierdzi. Dotyczy to także skryptu napisanego przez agenta.

**Wykonanie i niezawodność**
- R12. Skrypt ma stały kontrakt: dostaje warunek i zapamiętany stan z poprzedniego uruchomienia, wykonuje skutki przez `narsor-cli` (`task create`, `task input`) i zwraca nowy stan. Każdy skutek ma stały klucz (na przykład `ABC-123` albo `PR#42:review:1234`), który skrypt przekazuje do `narsor-cli`.
- R13. Narsor odrzuca drugie wywołanie skutku z tym samym kluczem w jednym obserwatorze. Dotyczy to też ponownego uruchomienia po awarii.
- R14. Uruchomienie skryptu ma limit czasu. Błąd, przekroczenie czasu lub zły format stanu nie zmienia zapamiętanego stanu obserwatora. Skutki, które skrypt wykonał przed błędem, zostają; deduplikacja z R13 chroni przed ich powtórzeniem w kolejnym uruchomieniu.
- R15. Jedno uruchomienie ma limit liczby nowych tasków. Narsor odrzuca wywołania `task create` ponad limit, a obserwator przechodzi w stan wymagający uwagi operatora.
- R16. Kolejne błędy uruchomień zwiększają odstęp między próbami. Po ustalonej liczbie błędów obserwator przechodzi w stan „w błędzie" i operator to widzi.
- R17. Harmonogram ma dolną granicę odstępu między uruchomieniami (ochrona limitów API źródeł).

**Widoczność i bezpieczeństwo**
- R18. Operator widzi listę obserwatorów, ich stan, czas ostatniego uruchomienia, wynik i błąd. Operator może wstrzymać, wznowić i usunąć obserwator przez CLI. TUI w późniejszej wersji.
- R19. Task utworzony przez obserwator i zdarzenie przekazane do taska mają ślad pochodzenia: nazwa obserwatora i klucz zmiany.
- R20. Dane dostępowe do źródeł pochodzą z rejestru sekretów control plane. Skrypt nie zawiera sekretów. Wynik przebiegu próbnego i logi nie pokazują sekretów. Skrypt używa tokenu Narsora ograniczonego do jego obserwatora (tylko `task create` i `task input`, ważnego w czasie uruchomienia).
- R21. W trybie sieciowym tylko właściciel instalacji tworzy, zmienia i zatwierdza obserwatory.

---

## Acceptance Examples

- AE1. **Covers R5, R6.** Given obserwator PR bez wpisów, when nadchodzi czas harmonogramu, then skrypt nie startuje i nie ma wpisu o uruchomieniu.
- AE2. **Covers R12, R13.** Given zadanie `ABC-123` dało już task, when skrypt w kolejnym uruchomieniu znowu zwraca `ABC-123`, then Narsor nie tworzy drugiego taska.
- AE3. **Covers R13, R14.** Given skrypt utworzył 3 z 5 tasków i skończył się kodem błędu, when silnik czyta wynik, then stan obserwatora zostaje bez zmian, a w kolejnym uruchomieniu Narsor odrzuca powtórzone klucze i tworzy tylko brakujące 2 taski.
- AE4. **Covers R15.** Given limit 10 tasków na uruchomienie, when skrypt zwraca 250 zadań (na przykład błędny JQL), then Narsor odrzuca wywołania ponad limit (tworzy najwyżej 10 tasków), a obserwator czeka na decyzję operatora.
- AE5. **Covers R4, R7.** Given task obserwuje MR !17 na zdarzenie „approved", when recenzent zatwierdza MR, then task dostaje zdarzenie; when MR zostaje scalony, then wpis znika z listy.
- AE7. **Covers R4.** Given task nie czeka w kroku `wait`, when nadchodzi zdarzenie „approved", then zdarzenie trafia do historii, a task nie zmienia stanu; when task później wejdzie w krok `wait`, then nie widzi tego zdarzenia.
- AE6. **Covers R11.** Given agent zmienił skrypt aktywnego obserwatora, when nikt nie zatwierdził zmiany, then harmonogram dalej uruchamia poprzednią zatwierdzoną wersję.

---

## Success Criteria

- Operator nie przenosi ręcznie zadań z Jiry ani informacji o końcu review. Task z oznaczonego zadania powstaje w czasie jednego odstępu harmonogramu.
- Żadne zdarzenie zewnętrzne nie daje zduplikowanego taska ani zduplikowanego zdarzenia w tasku.
- Błędny skrypt lub awaria źródła nie tworzą tasków i są widoczne dla operatora.
- `ce-plan` nie musi wymyślać: kontraktu skryptu na poziomie zachowania, reguł deduplikacji, reguł końca obserwacji PR/MR ani zasad zatwierdzania skryptów.

---

## Scope Boundaries

- Brak wbudowanych integracji (klientów API) z Jirą, GitHub, GitLab w kodzie Narsora. Tylko skrypty i przykłady.
- Brak webhooków przychodzących w v1. Tylko odpytywanie według harmonogramu.
- Brak zapisu z Narsora do źródeł (na przykład komentarz w Jirze, zmiana statusu). To mogą robić kroki workflow.
- Brak edytora obserwatorów w TUI w v1. TUI może pokazać stan obserwatorów później.
- Brak innych źródeł niż Jira, GitHub, GitLab w przykładach v1. Silnik nie ogranicza źródeł.

---

## Key Decisions

- Silnik oparty o skrypty, nie o wbudowane integracje: zespoły różnią się warunkami i polami. Skrypt daje elastyczność bez zmian w kodzie Narsora.
- Skrypt sam wykonuje skutki przez `narsor-cli` (decyzja użytkownika, Q1): daje to elastyczność, na przykład wiele tasków z jednego zadania Jira. Cena: Narsor nie widzi zamiaru skryptu z góry. Dlatego deduplikację po kluczu (R13), limit tasków (R15) i ślad pochodzenia (R19) egzekwuje Narsor przy wywołaniu `narsor-cli`, a skrypt dostaje token o ograniczonym zakresie (R20).
- Zdarzenie dla taska, który nie czeka, tylko trafia do historii (decyzja użytkownika, Q3): `wait` widzi wyłącznie zdarzenia z czasu czekania.
- Wpis obserwacji PR/MR dodaje krok workflow (YAML) albo operator (decyzja użytkownika, Q4): obserwacja jest zwykle powiązana z krokami, które czekają na ocenę, decyzję lub review użytkownika.
- Obserwacja PR/MR jako lista wpisów powiązanych z taskami: spełnia wymaganie „tylko wskazane PR" i „bez PR skrypt nie startuje".
- Dostarczenie zdarzenia używa istniejących pojęć: `ExternalEventReceived` jako zapis w tasku i `WaitAction` jako krok, który czeka. Szczegóły w planowaniu.
- Człowiek zatwierdza każdy skrypt przed pracą według harmonogramu: skrypt agenta to kod wykonywany cyklicznie z dostępem do sekretów.

---

## Dependencies / Assumptions

- Działający wykonawca `WaitAction` jest warunkiem skutku „przekaż zdarzenie" z ciągiem dalszym w workflow. Dziś go nie ma.
- `narsor-cli task create` i `task input` są wywoływalne ze skryptu obserwatora z tokenem o ograniczonym zakresie. Niezweryfikowane.
- Rejestr sekretów control plane daje sekrety także poza taskami (dla obserwatorów). Niezweryfikowane.
- Środowisko uruchomienia skryptów ma `curl`, `jq` i dostęp sieciowy do źródeł.

---

## Założenia do potwierdzenia

- Z1. Skrypt wywołuje `narsor-cli` sam (decyzja Q1). Do potwierdzenia w planowaniu: czy `narsor-cli task create` przyjmuje klucz skutku i czy Narsor może pilnować limitu R15 na wywołaniach.
- Z2. W konfiguracji lokalnej skrypty działają na tej samej maszynie co `narsor-server`.
- Z3. Zdarzenie dostarczone do taska, który nie czeka w kroku `wait`, zostaje zapisane w historii i nie zmienia stanu taska (decyzja Q3).
- Z4. Każdy skrypt (także agenta) wymaga zatwierdzenia przez człowieka przed pracą według harmonogramu (R11).
- Z5. Domyślny odstęp harmonogramu to 5 minut. Dolna granica to 1 minuta.
- Z6. Domyślny limit nowych tasków na jedno uruchomienie to 10.
- Z7. W trybie sieciowym tylko właściciel instalacji zarządza obserwatorami.
- Z8. Obserwator Jira przenosi do taska tylko pola wskazane w mapowaniu (na przykład klucz, tytuł, link, opis). Zadania w Jirze mogą zawierać dane klientów. Twórca automatyzacji odpowiada za wybór pól. Zalecenie: nie przenosić pól z danymi osobowymi.
- Z9. Wpis obserwacji PR/MR kończy się automatycznie po zamknięciu lub scaleniu PR/MR albo po zakończeniu taska-adresata.
- Z10. Pierwsze uruchomienie obserwatora Jira nie importuje zaległych zadań, które już spełniają warunek. Importuje tylko zmiany od chwili aktywacji, chyba że twórca wybierze import zaległych.

---

## Pytania otwarte dla użytkownika

- P1. Rozstrzygnięte (Q1): skrypt sam wywołuje `narsor-cli`. Patrz Key Decisions.
- P2. [Affects R14, R20, Z2] Gdzie działają skrypty w konfiguracji Kubernetes?
  - (a) Rekomendowane: osobny pod (lub Job według harmonogramu), odizolowany od `narsor-server`, z dostępem tylko do sekretów obserwatora.
  - (b) W procesie lub kontenerze `narsor-server`.
  - (c) Na backendzie taska (runner), obok workspace.
- P3. Rozstrzygnięte (Q3): zdarzenie tylko w historii; `wait` widzi zdarzenia z czasu czekania.
- P4. Rozstrzygnięte (Q4): krok workflow (YAML) i operator przez CLI.

---

## Outstanding Questions

### Resolve Before Planning

- P2 z sekcji „Pytania otwarte dla użytkownika" (P1, P3, P4 rozstrzygnięte).

### Deferred to Planning

- [Affects R12][Technical] Dokładny format wyniku skryptu i zapamiętanego stanu.
- [Affects R4][Technical] Wykonawca `WaitAction`: składnia filtra, limit czasu, zachowanie po limicie czasu.
- [Affects R17][Technical] Gdzie żyje harmonogram: w control plane czy w osobnym komponencie; zachowanie po restarcie serwera.
- [Affects R9][Needs research] Czy tworzenie skryptu z agentem to wbudowany workflow Narsora (agent pisze → przebieg próbny → decyzja człowieka). Rekomendacja: tak, bo używa istniejących kroków `agent`, `script` i `decision`.
- [Affects R20][Needs research] Czy rejestr sekretów backendu wystarcza dla obserwatorów, czy potrzebny jest osobny zakres sekretów.
