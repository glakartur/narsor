---
date: 2026-10-18
topic: przekaz-kontekstu-miedzy-krokami
---

# Przekaz kontekstu między krokami workflow

## Summary

Prompt kroku może odwołać się do końcowej odpowiedzi innego, nazwanego kroku. Wyjście kroku może wymagać od agenta zmiennych, a gdy agent ich nie zwróci, silnik upomina się o nie w tej samej sesji. Krok może mieć opcjonalny limit wejść. Po osiągnięciu limitu zadanie zatrzymuje się i czeka na ręczną decyzję człowieka. `compound-engineering` jest pierwszym odbiorcą tych mechanizmów.

---

## Problem Frame

Workflow `compound-engineering` zapętlił się na przeglądzie kodu. Krok `code_review` działa w trybie `read-only`, dlatego nie może zapisać raportu do pliku. Krok zwraca tylko liczby (`data.findings`, `data.blockers`). Silnik nie przekazuje `data` do następnych kroków. Krok `fix_review_findings` dostaje polecenie „napraw ustalenia z ostatniego raportu”, ale raportu nie dostaje. Agent sam przegląda diff i naprawia inne problemy niż te, które znalazł recenzent. Następny przegląd znajduje coś nowego. Pętla `code_review → fix_review_findings → code_review` nie ma limitu, a przejścia są statyczne, dlatego może działać bez końca. Każdy obieg kosztuje dwa przebiegi agenta (do 1 h każdy) i rozszerza diff.

Dziś są tylko te kanały między krokami:

| Kanał | Czy następny agent go widzi |
|---|---|
| `variables` z końcowego JSON agenta lub skryptu | Tak, przez `{{ context.NAZWA }}` |
| Pola bramy `human` | Tak, jako zmienne |
| `resume_comment_field` | Tylko jako wiadomość przy wznowieniu sesji agenta docelowego |
| Pliki we wspólnym workspace | Tak, ale krok `read-only` nie może ich zapisać |
| `data`, `artifacts` | Nie, szablony ich nie obsługują |
| `last_agent_step` | Tylko brama `human`, która następuje bezpośrednio po kroku agenta |

Zmienne działają, ale tylko wtedy, gdy agent pamięta, że ma je zwrócić. Silnik nie sprawdza, czy agent zwrócił zmienną, której potrzebuje późniejszy krok. Gdy zmiennej brakuje, błąd pojawia się dopiero w kroku odbiorcy, przy rozwijaniu szablonu, a nie w kroku, który powinien ją zwrócić.

---

## Actors

- A1. Autor workflow: definiuje, które kroki przekazują wyniki, jakie zmienne są wymagane i gdzie jest limit wejść.
- A2. Agent kroku: zwraca odpowiedź i zmienne. Dostaje wyniki wcześniejszych kroków w prompcie.
- A3. Operator: podejmuje decyzję, gdy krok osiągnie limit wejść.
- A4. Silnik: rozwija szablony, sprawdza wymagane zmienne, liczy wejścia i zatrzymuje zadanie.

---

## Key Flows

- F1. Przekaz raportu przeglądu do naprawy
  - **Trigger:** `code_review` kończy się etykietą `findings`.
  - **Actors:** A2, A4
  - **Steps:** Recenzent kończy odpowiedź raportem i etykietą. Silnik zapisuje końcową odpowiedź tego wejścia. Silnik przechodzi do `fix_review_findings` i wstawia do jego promptu odpowiedź `code_review`. Agent naprawiający pracuje na tej liście ustaleń, bez własnego przeglądu.
  - **Outcome:** Naprawa dotyczy dokładnie tych ustaleń, które zgłosił recenzent.
  - **Covered by:** R1, R2, R3, R4, R14

- F2. Zatrzymanie na limicie wejść
  - **Trigger:** Przejście prowadzi do kroku, który ma limit, a licznik wejść tego kroku osiągnął limit.
  - **Actors:** A3, A4
  - **Steps:** Silnik nie startuje kroku i zatrzymuje zadanie. Operator widzi, który krok osiągnął limit i ile razy w niego wchodzono. Operator wybiera jedną decyzję: kontynuuj, kontynuuj z nowym licznikiem albo jedno z zadeklarowanych wyjść kroku. Silnik wykonuje decyzję.
  - **Outcome:** Żadna automatyczna pętla nie przekroczy limitu bez decyzji człowieka.
  - **Covered by:** R9, R10, R11, R12

---

## Requirements

**Odpowiedź nazwanego kroku w szablonach**
- R1. Szablon w prompcie kroku może odwołać się do końcowej odpowiedzi innego kroku po jego nazwie (identyfikatorze stanu).
- R2. Odwołanie zwraca odpowiedź z najnowszego wejścia w ten krok, które zakończyło się etykietą wyjścia. Wejście zakończone błędem akcji nie zastępuje wcześniejszej odpowiedzi.
- R3. Odpowiedź to tekst, który agent napisał na końcu wejścia, bez końcowego obiektu JSON kontraktu wyjścia. Gdy wejście miało dopytania lub upomnienia w tej samej sesji, liczy się odpowiedź z ostatniego przebiegu w tej sesji.
- R4. Odwołanie do kroku, który nie ma jeszcze żadnej odpowiedzi, jest błędem, który przerywa akcję. To ta sama zasada co dziś dla nieistniejącej zmiennej: silnik nie podstawia po cichu pustego tekstu.
- R5. Walidator workflow odrzuca odwołanie do kroku, którego nie ma w definicji, oraz do kroku, który nie jest krokiem agenta.

**Wymagane zmienne na wyjściu**
- R6. Definicja wyjścia kroku agenta może zadeklarować zmienne, które agent musi zwrócić razem z tą etykietą.
- R7. Gdy agent zwróci etykietę bez wymaganej zmiennej (brak klucza lub pusta wartość), silnik upomina się o nią w tej samej sesji, tak jak dziś przy braku etykiety (KTD7). Upomnienie nazywa brakujące zmienne. Gdy agent nadal ich nie zwróci, wejście kończy się błędem akcji.
- R8. Kontrakt wyjścia doklejany przez silnik do promptu wymienia wymagane zmienne przy każdej etykiecie, tak jak dziś wymienia etykiety. Autor workflow nie musi ich powtarzać w prompcie.

**Limit wejść w krok**
- R9. Definicja kroku może mieć opcjonalny limit wejść. Bez limitu krok działa tak jak dziś.
- R10. Wejście to start kroku z nową sesją harnessu i z promptem z definicji kroku, wywołany przejściem workflow. Licznik nie zmienia się przy:
  - wznowieniu tej samej sesji po odpowiedzi człowieka (`awaiting_input`),
  - zatwierdzeniu lub odrzuceniu uprawnień,
  - upomnieniu o etykietę lub o wymagane zmienne,
  - odrzuceniu wyniku do poprawy (`requires_human_approval`),
  - wznowieniu przez `resume_comment_field`,
  - ponownej próbie z `retry_policy`.
- R11. Licznik obejmuje całe zadanie. Przejście przez bramę `human` go nie zeruje.
- R12. Gdy przejście prowadzi do kroku, którego licznik osiągnął limit, silnik nie startuje kroku i zatrzymuje zadanie w stanie oczekiwania na decyzję operatora. Operator ma do wyboru:
  - **kontynuuj**: krok startuje jeden raz ponad limit, a licznik rośnie. Następne wejście znowu się zatrzyma.
  - **kontynuuj z nowym licznikiem**: licznik się zeruje, a krok startuje jako pierwsze wejście.
  - **dowolne zadeklarowane wyjście kroku**: krok nie startuje, a workflow idzie dalej przejściem tej etykiety.

  Anulowanie zadania zostaje dostępne jak zawsze, jako zwykła operacja operatora.
- R13. Zatrzymanie, decyzja operatora i zerowanie licznika są zdarzeniami w dzienniku zadania. Licznik przetrwa restart daemona.

**Pierwszy odbiorca: `compound-engineering`**
- R14. `fix_review_findings` dostaje w prompcie odpowiedź `code_review` i ma wyraźny zakaz własnego przeglądu diffu.
- R15. `code_review` ma limit wejść, który zostawia zapas na kilka rund poprawek człowieka. Autor wybiera wartość przy zmianie workflow.
- R16. Istniejące zmienne, od których zależą późniejsze kroki (`requirements_doc`, `plan_doc`, `pr_url`), są wymagane na właściwych wyjściach.
- R17. Krok `work` zwraca stan bazowy testów (testy, które nie przechodziły przed zmianą), a `fix_review_findings` i `work_revision` dostają go w prompcie. Te kroki nie naprawiają takich testów i nie badają ich ponownie.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R14.** Mamy przebieg, w którym `code_review` zwrócił `findings` dwa razy. Gdy `fix_review_findings` startuje drugi raz, jego prompt zawiera raport z drugiego przeglądu, a nie z pierwszego.
- AE2. **Covers R2.** Mamy przebieg, w którym drugie wejście w `code_review` skończyło się błędem akcji, a operator ręcznie przeszedł do `fix_review_findings`. Prompt zawiera raport z pierwszego, udanego wejścia.
- AE3. **Covers R4.** Mamy workflow, w którym prompt kroku `b` odwołuje się do odpowiedzi kroku `a`. Gdy operator ręcznie przejdzie do `b`, zanim `a` kiedykolwiek się wykonał, akcja `b` kończy się błędem z nazwą brakującego kroku.
- AE4. **Covers R6, R7.** Mamy krok `plan` z wymaganą zmienną `plan_doc`. Gdy agent zwraca `plan_ready` bez `plan_doc`, silnik w tej samej sesji prosi o `plan_doc`. Gdy agent ją dosyła, przejście wykonuje się normalnie, a licznik wejść `plan` się nie zmienia.
- AE5. **Covers R10.** Mamy krok `work` z limitem 2. Agent dwa razy pyta człowieka (`awaiting_input`) i raz czeka na uprawnienia. Licznik `work` po tym wejściu wynosi 1.
- AE6. **Covers R10.** Mamy krok `code_review` z `retry_policy.max_attempts: 2` i limitem 3. Pierwsza próba kończy się błędem, druga etykietą. Licznik wynosi 1.
- AE7. **Covers R11, R12.** Mamy krok `code_review` z limitem 4, w który wchodzono już 4 razy, w tym 2 razy po decyzji „revise” na bramie `implementation_review`. Przejście `fix_review_findings → code_review` zatrzymuje zadanie. Operator wybiera „kontynuuj”. `code_review` startuje, licznik wynosi 5, a następne wejście znowu się zatrzyma.
- AE8. **Covers R12.** W tej samej sytuacji operator wybiera wyjście `clean`. `code_review` nie startuje, a workflow przechodzi do `publish_work`.

---

## Success Criteria

- Przebieg `compound-engineering` z ustaleniami CR kończy pętlę przegląd–naprawa, gdy recenzent przestaje zgłaszać ustalenia. Agent naprawiający nie pisze już, że raport do niego nie trafił.
- Żadna automatyczna pętla w workflow z limitem nie przekroczy limitu bez decyzji operatora.
- Brak wymaganej zmiennej widać w kroku, który powinien ją zwrócić, a nie dopiero w kroku odbiorcy.
- `ce-plan` może zaplanować zmianę bez wymyślania, co znaczy „odpowiedź kroku”, „wejście” i „decyzja na limicie”.

---

## Scope Boundaries

- Dziennik wspólnej wiedzy zadania jest odłożony. To narastające notatki, które silnik dokleja do każdego promptu. Dziś mamy jeden przypadek (stan bazowy testów), a R17 obsługuje go przez zmienną. Gdy wrócimy do tematu, zabezpieczenie przed nadużyciem to budżet i kryteria w kontrakcie, bez dyrygenta. Warunek powrotu: 2–3 nowe przypadki innego rodzaju niż stan bazowy testów.
- Kontynuacja sesji wskazanego kroku (np. `plan → plan_revision`) to osobny temat. Silnik ma już na nią przygotowane `agent_session_id`.
- Dyrygent nie wyciąga wniosków między krokami i nie porządkuje treści przekazu.
- Silnik nie przekazuje `data` ani `artifacts` w szablonach. Kroki `read-only` nie dostają katalogu do zapisu raportów.
- Dopisywanie do zmiennych zamiast nadpisywania jest poza zakresem. To była potrzeba dziennika wspólnej wiedzy.
- Odwołanie do „poprzedniego kroku” bez nazwy jest poza zakresem.

---

## Key Decisions

- Odwołanie do nazwanego kroku, a nie do poprzedniego: po pętlach, bramach `human` i ręcznych przejściach „poprzedni” krok jest niejednoznaczny.
- Końcowa odpowiedź zamiast całej rozmowy lub kilku ostatnich wypowiedzi: w trybie `report-only` raport jest końcową odpowiedzią. Cała rozmowa to szum i duży koszt w prompcie.
- Wymagane zmienne z upomnieniem w tej samej sesji: to ten sam mechanizm co przy braku etykiety (KTD7). Agent naprawia tylko format, a wykonana praca zostaje.
- Ochrona przed pętlą jako część tego samego zakresu: lepszy przekaz nie zatrzyma pętli, w której recenzent przy każdym obiegu znajduje coś nowego.
- Licznik liczy tylko nowe wejścia i obejmuje całe zadanie, bez zerowania: wznowienia i ponowne próby nie są nowym obiegiem pętli. Wartość limitu wybiera autor, z zapasem na rundy poprawek człowieka.
- Decyzja na limicie bez osobnej bramy w workflow: autor nie musi definiować bramy `human` dla każdego kroku z limitem. Operator wybiera spośród wyjść, które krok już deklaruje.
- Dziennik wspólnej wiedzy jest odłożony: jeden zaobserwowany przypadek, stały koszt w każdym prompcie i ryzyko nadużyć ze strony agentów.

---

## Dependencies / Assumptions

- Wymagane zmienne korzystają z mechanizmu upomnienia o brak etykiety (KTD7) i rozszerzają go.
- Operator już dziś ma ręczne przejścia. Decyzja na limicie powinna z nich korzystać, a nie tworzyć drugi, równoległy mechanizm.
- Założenie (niesprawdzone): końcowa odpowiedź agenta claude-code jest dostępna jako tekst także wtedy, gdy adapter wymusza strukturę odpowiedzi schematem JSON (`--json-schema`).

---

## Outstanding Questions

### Deferred to Planning

- [Affects R3][Needs research] Gdy claude-code działa z `--json-schema`, czy raport recenzenta jest w tekście końcowym, czy tylko we wcześniejszej wiadomości asystenta? Od tego zależy, co silnik zapisuje jako „odpowiedź kroku”. To samo trzeba sprawdzić dla codex i opencode.
- [Affects R1, R3][Technical] Czy odpowiedź kroku ma limit rozmiaru w prompcie? Jeśli tak, prompt musi wyraźnie mówić, że tekst jest skrócony.
- [Affects R1][Technical] Składnia odwołania w szablonie. Musi pasować do obecnych form `inputs.*` i `context.variables.*`.
- [Affects R12][Technical] Jak zatrzymanie na limicie wygląda w TUI i API. Może to być status zbliżony do `input_needed` albo osobny status.
- [Affects R13][Technical] Gdzie silnik trzyma licznik wejść, aby przetrwał restart i był spójny z dziennikiem zdarzeń (D10, D23).
- [Affects R4][Technical] Czy walidator może ostrzec, że krok odbiorca jest osiągalny przed krokiem źródłowym.
