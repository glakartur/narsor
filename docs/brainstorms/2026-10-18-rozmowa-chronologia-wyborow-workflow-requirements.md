---
date: 2026-10-18
topic: rozmowa-chronologia-wyborow-workflow
---

# Chronologiczne miejsce wyborów workflow w zakładce Rozmowa

## Summary

W zakładce *Rozmowa* (szczegóły zadania oraz Historia) każdy wpis „WYBÓR WORKFLOW” jest w miejscu, które odpowiada czasowi wyboru. Wpis jest między rozmowami kroków, a nie w jednym bloku na końcu transkryptu.

## Problem Frame

Zakładka *Rozmowa* łączy rozmowy wszystkich kroków zadania w jeden transkrypt. Każda rozmowa kroku zaczyna się od nagłówka `── <krok> ──`. Do transkryptu TUI dodaje też wpisy „WYBÓR WORKFLOW · HH:MM:SS”. Te wpisy pokazują, kto wybrał wyjście workflow: agent (`DecisionMade`), dyrygent (`ConductorDecision` typu `ChooseOutput`) lub użytkownik (`HumanInputReceived` z etykietą wyjścia, `HumanTransitionSelected`).

**Pojęcia.** *Krok* to stan workflow (np. `code_review`). *Wejście w krok* zaczyna się, gdy workflow przechodzi do tego kroku z innego kroku. Wejście kończy się, gdy workflow przechodzi do następnego kroku. Wznowienie pracy na tym samym kroku nie zaczyna nowego wejścia. Przykłady wznowień: odpowiedź na pytanie agenta, wznowienie po zabiciu agenta lub sesji, retry, ponowne uruchomienie agenta na tym samym kroku z innego powodu. Jedno wejście może mieć kilka prób (attempt) i kilka pozycji osi. Ta reguła jest taka sama jak reguła licznika `max_entries`: licznik rośnie tylko przy przejściu między krokami. W pętli `code_review → fix_review_findings → code_review` są dwa wejścia w `code_review`.

Dziś `Conversation::rebuild_with_events` w `src/tui/task_app/tabs/conversation.rs` dodaje wszystkie wybory po rozmowie ostatniego kroku. Komentarz w kodzie mówi, że to jest celowe: wybory mają być widoczne po otwarciu rozmowy na jej końcu. Skutek: operator czyta, że krok `code_review` się skończył, ale nie widzi, jaką decyzję podjęto po nim. Wszystkie decyzje z całego zadania są dopiero na samym dole i operator musi je sam dopasować do kroków według godziny. W długim zadaniu z pętlami (np. `code_review → fix_review_findings → code_review`) nie można w ten sposób odczytać przebiegu.

## Requirements

**Kolejność**
- R1. Wpis wyboru workflow jest w transkrypcie w kolejności czasu wyboru, względem rozmów kroków. Wybór, który nastąpił po wejściu w krok A i przed wejściem w krok B, jest po ostatnim wpisie rozmowy z wejścia w krok A (wszystkich jego prób) i przed nagłówkiem kroku B.
- R1a. Wznowienie pracy na tym samym kroku (patrz *Pojęcia*) nie jest granicą dla wpisu wyboru. Wybór nie jest między próbami jednego wejścia w krok.
- R2. Gdy kilka wyborów jest między tymi samymi dwoma krokami, ich kolejność jest kolejnością zdarzeń (numer sekwencji zdarzenia).
- R3. Wybór, który nastąpił przed pierwszym krokiem z rozmową, jest przed nagłówkiem tego kroku. Wybór po ostatnim kroku jest na końcu transkryptu (jak dziś).
- R4. Wybory po krokach bez zapisanej rozmowy (np. brama `human`, krok skryptu) też mają miejsce wg R1: między rozmowami sąsiednich kroków z rozmową.

**Bez zmian**
- R5. Treść, nagłówek, kolor i identyfikator wpisu wyboru są bez zmian. Zestaw zdarzeń, które dają wpis wyboru, jest bez zmian.
- R6. Zmiana działa tak samo w szczegółach aktywnego zadania i w Historii (oba miejsca wywołują ten sam mechanizm budowy transkryptu).
- R7. Kursor, rozwinięte wpisy i tryb „podąża” zachowują się jak dziś po przebudowie transkryptu. Nowy wybór, który dochodzi na końcu, jest widoczny dla czytelnika w trybie „podąża”.

## Acceptance Examples

- AE1. **Covers R1, R2.** Dane: kroki `plan` → `code_review` → `fix_review_findings`. Agent `code_review` wybrał `revise`, a potem dyrygent wybrał `fix`. Wynik: transkrypt to nagłówek `plan`, rozmowa `plan`, nagłówek `code_review`, rozmowa `code_review`, wybór agenta `revise`, wybór dyrygenta `fix`, nagłówek `fix_review_findings`, rozmowa `fix_review_findings`.
- AE2. **Covers R4.** Dane: krok agenta `plan`, potem brama `human`, na której użytkownik wybrał `approved`, potem krok agenta `implement`. Wynik: wybór użytkownika `approved` jest między rozmową `plan` a nagłówkiem `implement`.
- AE3. **Covers R1.** Dane: pętla `code_review → fix → code_review`. Wynik: każdy wybór jest po rozmowie tego wejścia w `code_review`, którego dotyczy, a nie po ostatnim wejściu.
- AE5. **Covers R1a.** Dane: w kroku `implement` agent zadaje pytanie, użytkownik odpowiada, agent wznawia pracę. Potem sesję zabito i agent ponownie wznowił pracę na `implement`. Na końcu agent wybrał `done`, a następny krok to `code_review`. Wynik: wszystkie próby `implement` są jednym wejściem. Wybór `done` jest po ostatnim wpisie ostatniej próby `implement` i przed nagłówkiem `code_review`.
- AE4. **Covers R3, R7.** Dane: zadanie czeka na decyzję po ostatnim kroku, operator jest na końcu transkryptu w trybie „podąża”. Operator wybiera wyjście. Wynik: nowy wpis wyboru jest na końcu i jest widoczny bez przewijania.

## Success Criteria

- Operator czyta *Rozmowę* od góry do dołu i widzi przebieg zadania w tej kolejności, w jakiej przebieg nastąpił: rozmowa kroku, decyzja, następny krok.
- Na końcu transkryptu nie ma bloku wyborów z całego zadania.
- Test jednostkowy `rebuild_with_events` sprawdza kolejność dla scenariuszy AE1–AE3 i AE5.

## Scope Boundaries

- Nie zmieniamy treści ani formatu wpisu „WYBÓR WORKFLOW”.
- Nie dodajemy nowych typów zdarzeń do transkryptu (np. start/koniec kroku, retry).
- Nie wstawiamy wyboru w środek rozmowy kroku. Ziarno kolejności to granica między rozmowami kroków.
- Nie zmieniamy zakładek *Przebieg*, *Zdarzenia* i *Dyrygent*.

## Key Decisions

- Ziarno: granica między krokami, a nie pojedynczy wpis rozmowy. Wybór workflow zawsze następuje po zakończeniu kroku, dlatego granica kroków wystarcza. Nie potrzebujemy znaczników czasu wpisów rozmowy z różnych harnessów.
- Jednostka kolejności to wejście w krok, a nie próba. Wstrzymanie i wznowienie pracy na kroku to normalne działanie procesu, dlatego nie zmienia granic w transkrypcie (ta sama reguła jak dla licznika `max_entries`).
- Rezygnujemy z reguły „wybory zawsze na końcu”. Najnowszy wybór i tak jest na końcu, gdy po nim nie ma nowego kroku (R3, AE4).

## Dependencies / Assumptions

- Zakładamy, że czas zdarzenia (`Event.timestamp`) i czas startu pozycji osi (`Position.started_at`) pochodzą z tego samego zegara silnika i można je porównać. Do weryfikacji w planowaniu.

## Outstanding Questions

### Deferred to Planning

- [Affects R1, R4][Technical] Którą kotwicę użyć: czas wyboru względem `started_at`/`ended_at` pozycji, czy powiązanie przez `state_id`/epizod zdarzenia z konkretną pozycją? `DecisionMade` nie ma `state_id`, dlatego kotwica czasowa wygląda na prostszą.
- [Affects R1] Co zrobić z pozycją bez `started_at` (np. krok w toku z niepełnymi danymi): przyjąć czas następnej znanej pozycji?
- [Affects R1, R1a][Technical] Jak w danych osi (pozycje, próby, `Timeline.links`) rozpoznać granicę wejścia w krok, czyli przejście z innego kroku, a nie wznowienie? Czy można użyć tego samego źródła co licznik `max_entries`?
