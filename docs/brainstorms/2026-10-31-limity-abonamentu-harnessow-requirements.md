---
date: 2026-10-31
topic: limity-abonamentu-harnessow
---

# Wykorzystanie limitów abonamentu harnessów w TUI i CLI

## Summary

Runner wyczytuje stan limitów abonamentu z komunikatów harnessu i wysyła go do `narsor-server`. Serwer trzyma ostatni stan w pamięci i udostępnia go przez REST API. TUI pobiera stan co 15 s i pokazuje go w dolnym wierszu, wyrównany do prawej. Nowa komenda CLI pokazuje ten sam stan. Komunikaty limitów nie trafiają już do logu rozmowy.

---

## Problem Frame

Claude Code wysyła w strumieniu komunikat `rate_limit_event`. Komunikat podaje wykorzystanie limitu 5-godzinnego i 7-dniowego, czas odnowienia każdego limitu i status (np. `allowed_warning`). Dziś klasyfikator Claude Code (`crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs`, funkcja `rate_limit`) zamienia ten komunikat na wpis techniczny `RateLimit` w rozmowie kroku. Wpis zostaje w logu na stałe, chociaż po kilku minutach jest nieaktualny.

Operator nie widzi stanu limitów w jednym miejscu. Aby wiedzieć, czy abonament wystarczy na kolejne zadania, musi otworzyć rozmowę kroku i znaleźć ostatni wpis techniczny. Gdy limit 7-dniowy jest blisko 100%, operator dowiaduje się o tym zbyt późno — dopiero gdy harness odrzuca pracę.

Narsor obsługuje kilka harnessów (`claude_code`, `codex`, `opencode`, `github_copilot` w `crates/narsor-engine/src/engine/actions/harness/`). Dziś tylko Claude Code ma znany komunikat o limitach.

---

## Actors

- A1. Operator: używa TUI lub CLI. Decyduje, czy uruchomić nowe zadania.
- A2. Harness (np. Claude Code): wysyła komunikaty o limitach w swoim strumieniu.
- A3. `narsor-runner`: wykonuje krok agenta, czyta strumień harnessu, raportuje do serwera (strona aktywna).
- A4. `narsor-server`: właściciel kontraktu limitów. Trzyma ostatni stan w pamięci i udostępnia go klientom.

---

## Key Flows

- F1. Raport limitów
  - **Trigger:** harness wysyła komunikat o limitach w czasie kroku agenta.
  - **Actors:** A2, A3, A4
  - **Steps:** runner rozpoznaje komunikat → zamienia go na raport w kontrakcie serwera → wysyła raport do serwera → nie zapisuje komunikatu w logu rozmowy → serwer zastępuje poprzedni stan tego harnessu.
  - **Outcome:** serwer ma najnowszy stan limitów harnessu. Log rozmowy nie ma wpisu o limitach.
  - **Escape:** serwer jest niedostępny → runner pomija raport, krok działa dalej bez błędu.
  - **Covered by:** R1–R8
- F2. Odczyt w TUI
  - **Trigger:** co 15 s oraz przy starcie TUI.
  - **Actors:** A1, A4
  - **Steps:** TUI pyta serwer o stan harnessów → buduje krótki tekst → pokazuje go po prawej stronie dolnego wiersza.
  - **Outcome:** operator widzi aktualny stan limitów bez otwierania rozmów.
  - **Covered by:** R9–R16
- F3. Odczyt w CLI
  - **Trigger:** operator uruchamia komendę limitów.
  - **Actors:** A1, A4
  - **Steps:** CLI pyta serwer → wypisuje stan każdego harnessu i każdego limitu.
  - **Covered by:** R17–R19

---

## Requirements

**Kontrakt serwera**

- R1. `narsor-server` definiuje jeden kontrakt raportu limitów, wspólny dla wszystkich harnessów. Kontrakt nie zawiera pól specyficznych dla Claude Code jako obowiązkowych.
- R2. Raport identyfikuje harness (np. `claude-code`). Stan każdego harnessu jest osobny.
- R3. Raport zawiera listę limitów. Każdy limit ma: typ limitu (ogólna nazwa okna, np. `5h`, `7d`), wykorzystanie jako ułamek 0–1, czas następnego odnowienia (znacznik czasu UTC) i opcjonalny status (np. dozwolony / ostrzeżenie / odrzucony). TUI i CLI liczą procent przy wyświetlaniu.
- R4. Raport zawiera czas, w którym runner odebrał komunikat od harnessu.
- R5. Serwer trzyma w pamięci tylko ostatni stan każdego harnessu. Nowy raport zastępuje poprzedni. Po restarcie serwera stan jest pusty, aż przyjdzie nowy raport.
- R6. Serwer wystawia endpoint odczytu stanu wszystkich harnessów. Odczyt wymaga tej samej autoryzacji co inne odczyty klienta. Przyjęcie raportu wymaga autoryzacji runnera (jak inne endpointy `/runner/v1/...`).
- R6a. Serwer sprawdza raport przed zapisem. Serwer odrzuca raport z nieznanym harnessem. Status jest wartością z zamkniętego zbioru. Typ limitu ma stały wzorzec i krótki limit długości. Wykorzystanie mieści się w 0–1. Liczba limitów w raporcie i rozmiar żądania mają górną granicę. TUI i CLI nie wypisują znaków sterujących z danych raportu.

**Runner**

- R7. Runner rozpoznaje komunikat `rate_limit_event` Claude Code. Mapuje okna `five_hour` → `5h` i `seven_day` → `7d` z `unifiedWindows`. Gdy `unifiedWindows` nie ma, runner bierze jedno okno z `rateLimitType`, `utilization`, `resetsAt` i `status`. Status okna pochodzi z pola `status` tego okna w `unifiedWindows`. Gdy okno nie ma tego pola, a jest oknem z `rateLimitType`, runner bierze `status` z poziomu komunikatu. W innym przypadku limit nie ma statusu.
- R8. Runner wysyła raport do serwera i nie zapisuje komunikatu o limitach w logu rozmowy ani w transkrypcie kroku. Runner nie trzyma raportu po wysłaniu. Błąd wysłania nie przerywa ani nie oblewa kroku.

**TUI — dolny wiersz**

- R9. TUI pobiera stan limitów z serwera przy starcie i potem co 15 s. Pobieranie nie blokuje interfejsu.
- R10. TUI pokazuje stan w dolnym wierszu (wiersz podpowiedzi klawiszy), wyrównany do prawej, z co najmniej jedną spacją odstępu od podpowiedzi.
- R11. Format jest skrócony i czytelny. Dla każdego harnessu: krótka nazwa, a dla każdego limitu typ, procent i czas do odnowienia. Przykład: `claude 5h 1% ↻4h10m · 7d 90% ↻2d3h`. Czas odnowienia krótszy niż 24 h ma postać `XhYm`, dłuższy `XdYh`.
- R12. Limit ze statusem ostrzeżenia ma kolor ostrzeżenia. Limit odrzucony ma kolor błędu. Bez koloru (`NO_COLOR`) stan ostrzeżenia lub odrzucenia jest oznaczony znakiem `!`.
- R13. Gdy tekst limitów i podpowiedzi nie mieszczą się razem, TUI ukrywa podpowiedzi klawiszy, a nie limity. Gdy sam tekst limitów nie mieści się w wierszu, TUI najpierw usuwa całe segmenty ostatniego harnessu. Gdy tekst nadal się nie mieści, TUI obcina koniec tekstu i dodaje `…`. Gdy powiadomienie jest widoczne i nie mieści się obok podpowiedzi klawiszy, TUI ukrywa podpowiedzi klawiszy.
- R14. Powiadomienie (dziś też wyrównane do prawej w dolnym wierszu) ma pierwszeństwo przed limitami w czasie, gdy jest widoczne. Po zniknięciu powiadomienia limity wracają.
- R15. Gdy serwer nie ma danych o żadnym harnessie, TUI nic nie pokazuje w miejscu limitów. Gdy odczyt z serwera nie udał się, TUI pokazuje ostatni znany stan. Gdy czas odnowienia minął, TUI pokazuje `↻?` zamiast ujemnego czasu.
- R16. Limity są widoczne w każdym widoku TUI, który ma wiersz podpowiedzi klawiszy (lista zadań, szczegóły zadania, Historia), z wyjątkiem widoków ze stałym tekstem podpowiedzi (potwierdzenie wyjścia, ekran otwierania).

**CLI**

- R17. Nowa komenda CLI pobiera stan limitów z serwera i wypisuje go.
- R18. Wynik pokazuje dla każdego harnessu i każdego limitu: typ, wykorzystanie w procentach, status, czas odnowienia (lokalny czas i czas do odnowienia) oraz czas raportu.
- R19. Gdy serwer nie ma danych, komenda wypisuje jasny komunikat i kończy się kodem 0.

---

## Acceptance Examples

- AE1. **Covers R7, R8.** Dane: Claude Code wysyła `rate_limit_event` z `unifiedWindows` (5h 0.01 `allowed`, 7d 0.9 `allowed_warning`). Wynik: serwer ma dla `claude-code` dwa limity: `5h` 1% `allowed` i `7d` 90% ostrzeżenie. Rozmowa kroku nie ma wpisu `RateLimit`.
- AE2. **Covers R5.** Dane: dwa raporty dla `claude-code` w odstępie minuty. Wynik: endpoint odczytu zwraca tylko drugi raport.
- AE3. **Covers R8.** Dane: serwer jest niedostępny w chwili raportu. Wynik: krok agenta działa dalej i kończy się normalnie.
- AE4. **Covers R10, R11, R12.** Dane: terminal ma 160 kolumn, stan z AE1. Wynik: podpowiedzi po lewej, po prawej `claude 5h 1% ↻4h10m · 7d 90% ↻2d3h`, segment `7d` ma kolor ostrzeżenia.
- AE5. **Covers R13.** Dane: terminal ma 50 kolumn. Wynik: podpowiedzi klawiszy są ukryte, wiersz pokazuje tylko limity.
- AE6. **Covers R14.** Dane: TUI pokazuje powiadomienie. Wynik: powiadomienie zajmuje prawą stronę. Po zniknięciu powiadomienia limity są widoczne.
- AE7. **Covers R15.** Dane: serwer po restarcie, żaden runner nie raportował. Wynik: dolny wiersz ma tylko podpowiedzi. CLI wypisuje „brak danych o limitach” i kod 0.

---

## Success Criteria

- Operator widzi w TUI wykorzystanie limitów każdego harnessu bez otwierania rozmów kroków.
- Operator przez CLI uzyskuje ten sam stan, co TUI.
- Log rozmowy nie rośnie o wpisy o limitach.
- Planowanie nie musi wymyślać pól kontraktu, formatu skrótu ani zachowania przy braku miejsca.

---

## Scope Boundaries

- Brak trwałego zapisu i historii limitów na serwerze (tylko ostatni stan w pamięci).
- Brak alarmów i blokowania nowych zadań przy wysokim wykorzystaniu.
- Brak parserów limitów dla `codex`, `opencode` i `github_copilot` w tej zmianie. Kontrakt jest gotowy na nie.
- Brak usuwania wpisów `RateLimit`, które już są w zapisanych rozmowach. Stare rozmowy wyświetlają się jak dziś.
- Brak pól `overage*` (`overageStatus`, `isUsingOverage`, `overageDisabledReason`) w kontrakcie.
- Brak konfiguracji interwału 15 s przez użytkownika.

---

## Key Decisions

- Serwer jest właścicielem kontraktu, runner go wypełnia: jeden kontrakt dla wszystkich harnessów, mapowanie specyficzne dla harnessu jest w runnerze.
- Tylko ostatni stan w pamięci: dane szybko stają się nieaktualne, historia nie daje wartości operatorowi.
- Limity mają pierwszeństwo przed podpowiedziami klawiszy: to decyzja użytkownika z opisu zadania. Podpowiedzi są nadal dostępne pod `?`.
- Osobny interwał 15 s dla limitów: niezależny od `tui_refresh_interval`, bo limity zmieniają się wolno.

---

## Dependencies / Assumptions

- Runner ma już połączenie z serwerem i autoryzację (`/runner/v1/snapshots`, `/runner/v1/materials`). Nowy raport używa tej samej drogi.
- Zadania mogą działać przez lokalny controller i przez Kubernetes. W obu przypadkach runner raportuje do tego samego serwera.
- Lokalny daemon to `narsor-server` (`src/cli/daemon.rs`). `narsor run` też działa przez daemon i runner. Endpointy z R6 są więc dostępne także w trybie lokalnym.

---

## Założenia do potwierdzenia

1. Klucz stanu na serwerze to typ harnessu (np. `claude-code`). Gdy dwa runnery z różnymi kontami tego samego harnessu raportują, wygrywa ostatni raport. Rozróżnienie kont jest poza zakresem.
2. Interwał odczytu w TUI to dokładnie 15 s, bez konfiguracji.
3. Format skrótu w TUI jest jak w R11 (`claude 5h 1% ↻4h10m · 7d 90% ↻2d3h`). Procent jest zaokrąglony do liczby całkowitej.
4. Status `allowed_warning` dostaje kolor ostrzeżenia, status `rejected` (lub inny niż `allowed*`) kolor błędu.
5. Pola `overage*` z komunikatu Claude Code są pomijane.
6. Runner nie zapisuje komunikatu o limitach w logu rozmowy dla nowych kroków. Klasyfikator nie tworzy już wpisu `RateLimit`. Wspólna pętla stdout w wykonawcy agenta (`crates/narsor-engine/src/engine/actions/agent.rs`) nie zapisuje linii, którą parser harnessu oznaczył jako raport limitów. Ani surowa linia, ani wpis nie trafiają do pliku rozmowy. Typ `TechnicalCategory::RateLimit` zostaje, aby stare rozmowy czytały się bez błędu.
7. Gdy wysłanie raportu nie udało się, runner nie ponawia go. Następny komunikat harnessu da świeży stan.
8. Serwer nie usuwa starego stanu po czasie. Klient pokazuje czas raportu (CLI) lub `↻?` po minięciu odnowienia (TUI).
9. Nazwa komendy CLI to `narsor limits` (w grupie komend zdalnych, obok `workflows`). Komenda ma tryb `--json`, jeśli inne komendy listujące go mają.
10. Limity w TUI są widoczne także, gdy otwarty jest formularz lub dialog, jeśli dolny wiersz w tym widoku pokazuje podpowiedzi przez wspólną funkcję. Widoki ze stałym tekstem podpowiedzi (np. potwierdzenie wyjścia, ekran otwierania) nie pokazują limitów.

---

## Pytania otwarte dla użytkownika

1. Czy stan limitów ma rozróżniać konta (np. różne abonamenty na różnych controllerach), czy wystarczy jeden stan na typ harnessu?
2. Czy TUI ma ukrywać stan, który jest stary (np. raport starszy niż 7 dni), czy zawsze pokazywać ostatni znany?
3. Czy informacja o przekroczeniu limitu (`overage`) jest potrzebna w TUI lub CLI?
4. Czy wolisz inną nazwę komendy CLI niż `narsor limits` (np. `narsor usage`)?

---

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R3][Technical] Dokładny kształt JSON kontraktu i ścieżki endpointów (`/runner/v1/...` dla raportu, `/control/v1/...` dla odczytu).
- [Affects R7, R8][Technical] Gdzie runner przechwytuje komunikat: w klasyfikatorze silnika (`narsor-engine`) czy w warstwie runnera, i jak klasyfikator przekazuje raport bez wpisu do rozmowy.
- [Affects R9][Technical] Czy odczyt limitów używa istniejącego mechanizmu odświeżania (`src/tui/refresh.rs`) z osobnym zegarem.
- [Affects R13, R14][Technical] Jak połączyć limity, powiadomienie i podpowiedzi w funkcji rysującej dolny wiersz (`src/tui/task_app/layout.rs`, `src/tui/keymap.rs`).
