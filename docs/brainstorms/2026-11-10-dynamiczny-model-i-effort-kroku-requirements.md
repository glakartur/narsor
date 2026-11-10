---
date: 2026-11-10
topic: dynamiczny-model-i-effort-kroku
---

# Dynamiczny model i effort kroku workflow

## Summary

Każdy krok agenta dostaje model i effort z trzech źródeł, w tej kolejności: wartość domyślna z definicji workflow, profil zapisany przez dyrygenta po kroku planu, eskalacja po powtórzonych błędach. Dyrygent nie zgaduje złożoności przed planem. Przed planem działają wartości domyślne. Po planie dyrygent zapisuje profil raz.

Źródło pomysłu: `docs/follow-ups/2026-10-28-dobor-modelu-i-effortu-per-krok-workflow.md`. Ten dokument zamienia pomysł na wymagania.

---

## Problem Frame

Dziś krok agenta ma co najwyżej stały `model` w definicji workflow (przekazywany bez zmian do harnessu). Pola `effort` w definicji nie ma. Wszystkie zadania używają tych samych ustawień. Zadanie proste (zmiana jednej linii) kosztuje tyle samo co zadanie trudne (migracja i zmiana kontraktów).

Złożoność zadania jest znana dopiero po planie. Plan zawiera rozmiar jednostek pracy, zależności i ryzyka. Brainstorm i sam plan są więc kroki, dla których złożoność jeszcze nie istnieje. Autor workflow zna natomiast wagę każdego typu kroku (commit jest mechaniczny, code-review wymaga rozumowania).

---

## Actors

- A1. Autor workflow: zapisuje w definicji domyślny model i effort oraz górną granicę dla każdego kroku.
- A2. Dyrygent: czyta plan i zapisuje profil kroków. Nie pisze kodu.
- A3. Operator: widzi profil w TUI. Zatwierdza wyjście poza górną granicę.
- A4. Agent kroku: dostaje model i effort przy starcie kroku.

---

## Key Flows

- F1. Zadanie od początku do planu
  - **Trigger:** Start zadania.
  - **Actors:** A1, A4
  - **Steps:** Kroki przed planem i sam krok planu używają wartości domyślnych z workflow. Krok planu zwraca ocenę złożoności razem z planem.
  - **Outcome:** Plan i ocena złożoności istnieją. Profil jeszcze nie istnieje.
  - **Covered by:** R1, R2, R5

- F2. Profil po planie
  - **Trigger:** Krok planu kończy się sukcesem.
  - **Actors:** A2, A3
  - **Steps:** Dyrygent czyta plan i ocenę. Dla każdego kroku następnego zapisuje model i effort w granicach z workflow. Profil trafia do danych zadania i do TUI. Gdy dyrygent chce wyjść poza granicę, zadanie czeka na zgodę operatora. Gdy dyrygent nie jest pewny albo zawodzi, obowiązują wartości domyślne.
  - **Outcome:** Każdy krok ma profil. Operator widzi go przed startem kroku.
  - **Covered by:** R3, R4, R6, R7, R8

- F3. Eskalacja po błędach
  - **Trigger:** Ten sam krok kończy się błędem po raz kolejny.
  - **Actors:** A2
  - **Steps:** Próby do progu używają tego samego profilu. Po progu effort rośnie, potem model, w granicy z workflow.
  - **Outcome:** Krok używa mocniejszego profilu albo kończy się błędem, gdy granica jest osiągnięta.
  - **Covered by:** R9, R10

---

## Requirements

**Definicja workflow**
- R1. Krok agenta może mieć pole `effort` obok pola `model`. Oba pola są opcjonalne. Bez pól krok działa jak dziś.
- R2. Krok może mieć górną granicę modelu i effortu. Granica jest częścią definicji workflow, nie logiki dyrygenta.

**Profil po planie**
- R3. Po zakończeniu kroku planu dyrygent zapisuje profil (model i effort) dla kroków, które jeszcze nie ruszyły. Decyzja zapada raz.
- R4. Profil jest zapisany w danych zadania, wraz z krótkim uzasadnieniem dyrygenta. TUI pokazuje profil kroku.
- R5. Krok planu zwraca ocenę złożoności (np. liczba plików i modułów, migracje lub zmiany kontraktów, brak testów, zależności jednostek). Dyrygent opiera się na tej ocenie i na planie. Dyrygent nie ocenia ukrytej trudności.
- R6. Dyrygent może obniżyć model lub effort poniżej wartości domyślnej bez ograniczeń.
- R7. Dyrygent może podnieść model lub effort tylko do górnej granicy z R2. Wyjście poza granicę wymaga zgody operatora.
- R8. Gdy dyrygent nie jest pewny, albo jego wywołanie zawodzi, krok używa wartości domyślnej z workflow.

**Eskalacja**
- R9. Pierwsza i druga próba kroku używają tego samego profilu. Eskalacja zaczyna się po progu nieudanych prób. Próg jest parametrem konfiguracji.
- R10. Eskalacja podnosi najpierw effort, potem model, i nigdy nie przekracza granicy z R2.

**Jednostki pracy**
- R11. Dla kroku `work` wykonywanego w wielu jednostkach planu profil może być różny dla każdej jednostki (np. `ce-work: unit 3 → opus/high`).

---

## Acceptance Examples

- AE1. **Covers R3, R6.** Given plan z jedną małą zmianą w jednym pliku, when dyrygent zapisuje profil, then krok `work` dostaje model niższy niż domyślny.
- AE2. **Covers R7.** Given granica górna `opus/high` i propozycja `opus/max`, when dyrygent zapisuje profil, then zadanie czeka na zgodę operatora i do zgody używa `opus/high`.
- AE3. **Covers R8.** Given wywołanie dyrygenta kończy się błędem po planie, when krok `work` startuje, then używa wartości domyślnych z workflow.
- AE4. **Covers R9, R10.** Given próg 3 i krok, który kończy się błędem trzy razy z tym samym profilem, when startuje próba czwarta, then effort jest wyższy. Gdy effort jest już na granicy, rośnie model.
- AE5. **Covers R1.** Given workflow bez pól `effort`, when krok startuje, then harness dostaje te same argumenty co dziś.

---

## Success Criteria

- Zadanie proste zużywa mniej kosztu niż dziś. Zadanie trudne nie dostaje gorszego wyniku niż dziś. Do oceny: 2–3 realne zadania na różnych profilach, porównanie liczby rund przeglądu i kosztu.
- Operator widzi w TUI, jaki profil ma każdy krok i dlaczego.
- Plan wdrożenia nie musi wymyślać: kolejności źródeł profilu, reguły granic, ani reguły eskalacji.

---

## Scope Boundaries

- Dobór profilu przed krokiem planu na podstawie treści zadania. Powód: złożoność jest wtedy nieznana.
- Dyrygent nie pyta o profil przed każdym krokiem.
- Edycja profilu przez człowieka w bramce po planie: odroczona (patrz pytania otwarte).
- Automatyczne uczenie wartości domyślnych z historii zadań.
- Wartości domyślne z tabeli w dokumencie źródłowym są hipotezą. Ten dokument ich nie zatwierdza.

---

## Key Decisions

- Dwa etapy zamiast jednego: wartości domyślne obowiązują do planu, profil dyrygenta po planie. To odpowiada na pytanie "złożoność znana dopiero po planowaniu".
- Decyzja raz po planie, nie przed każdym krokiem: mniej wywołań dyrygenta i mniej niestabilności.
- Zejście w dół swobodne, wejście w górę ograniczone granicą: koszt może tylko spaść bez zgody człowieka.
- Eskalacja późna: błąd bywa przypadkowy.

---

## Dependencies / Assumptions

- Sprawdzone w kodzie: `model` jest polem kroku agenta (`crates/narsor-engine/src/domain/workflow_definition.rs`) i adapter Claude Code przekazuje go jako `--model` (`crates/narsor-engine/src/engine/actions/harness/claude_code.rs`). Pola `effort` w definicji kroku nie ma. Oś czasu zapisuje effort tylko wtedy, gdy argument `--effort` lub `--reasoning-effort` jest w wywołaniu (`crates/narsor-engine/src/engine/actions/agent.rs`).
- Niesprawdzone: czy Codex, OpenCode i Copilot obsługują effort i w jakiej formie.
- Niesprawdzone: w repozytorium `workflows/` nie ma pliku `compound-engineering.yaml`, o którym pisze dokument źródłowy. Gdzie leży ten workflow, trzeba ustalić.

---

## Założenia do potwierdzenia

- Zakres pierwszej wersji to kroki agenta. Kroki dyrygenta (jego własny model) są poza zakresem.
- Profil dotyczy tylko kroków, które jeszcze nie ruszyły po planie. Kroki już uruchomione nie zmieniają profilu.
- Brak profilu lub błąd dyrygenta nigdy nie blokuje zadania (R8).
- Ocenę złożoności zwraca krok planu jako dodatkowe dane. Dyrygent jej nie wylicza sam od zera.
- Harness bez obsługi effortu ignoruje to pole z widocznym ostrzeżeniem w zdarzeniach.
- Próg eskalacji domyślnie wynosi 3 (trzecia nieudana próba). Wartość do pomiaru.
- Górna granica jest wymagana tylko tam, gdzie autor chce ją mieć. Bez granicy dyrygent może podnieść ustawienie tylko do wartości domyślnej kroku.

---

## Pytania otwarte dla użytkownika

1. **Edycja profilu przez człowieka.** Czy operator ma móc zmienić profil po planie, przed startem kroków? Opcje: (a) tak, w istniejącej bramce po planie, (b) tylko podgląd w TUI, bez edycji, (c) edycja później, w osobnym zadaniu. Rekomendacja: (b) w pierwszej wersji.
2. **Granica bez zgody.** Co dzieje się, gdy dyrygent chce wyjść poza granicę, a operator nie odpowiada? Opcje: (a) zadanie czeka, (b) krok startuje na granicy i prośba zostaje otwarta, (c) krok startuje na wartości domyślnej. Rekomendacja: (b).
3. **Próg eskalacji.** Trzecia czy czwarta nieudana próba? Opcje: (a) trzecia, (b) czwarta, (c) parametr per krok. Rekomendacja: (a) jako parametr globalny.

## Deferred to Planning

- [Technical] Gdzie runtime przekazuje effort do każdego harnessu i jak mapować poziomy effortu na argumenty harnessu.
- [Technical] Kształt oceny złożoności zwracanej przez krok planu i miejsce zapisu profilu w danych zadania.
- [Needs research] Pomiar: które profile dają najlepszy stosunek kosztu do liczby rund przeglądu.
