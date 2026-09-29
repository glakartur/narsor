---
date: 2026-09-29
topic: dyrygent-i-obsluga-input-needed
---

# Dyrygent, odpowiedzi na input_needed i ratowanie brakującego labela

## Summary

Pod `i` operator odpowiada na każdy oczekujący input, a formularz sam pokazuje pytanie, które
zadał agent. TUI odświeża stan co ok. 10 s i sygnalizuje, ile tasków czeka na operatora,
odróżniając zwykłe pytania od problemów. Labele wyjścia dostają opcjonalne opisy, z których
silnik buduje listę dozwolonych odpowiedzi w prompcie i w dopytaniach. Gdy agent nie poda
labela, silnik raz dopytuje go w tej samej sesji. Jeśli to nie wystarczy, przejmuje
**dyrygent**: osobny, tylko do odczytu asystent na każdy task. Wybiera label, dopytuje agenta
albo eskaluje do operatora, diagnozuje błędy i ma własny czat.

---

## Problem Frame

Wykonanie `8c171de0` workflow `compound-engineering` pokazało kilka luk w obsłudze taska, który
się zatrzymał.

**Nie da się odpowiedzieć na bramkę human.** Klawisz `i` w TUI działa tylko wtedy, gdy agent sam
zadał pytanie w trakcie kroku (`awaiting_input`). Bramkę `human`, na przykład `resolve_ambiguity`,
da się zamknąć jedynie klawiszem `t`, który wysyła sam label. Wymagane pole tekstowe
(`ambiguity_answer`) nie ma więc skąd trafić do workflow, a następny krok (`apply_clarification`)
dostaje pustą odpowiedź zamiast decyzji operatora.

**Operator nie widzi pytania.** Zdarzenie `human_input_requested` niesie tylko statyczny
`message` z YAML-a („Read its question in the execution's data…”). Samo pytanie agent zapisał w
`data.question` poprzedniego kroku, a TUI go nie pokazuje. Operator musi wiedzieć, gdzie szukać,
i szukać ręcznie.

**Stan się nie odświeża.** Status tasków i widok taska aktualizują się dopiero po naciśnięciu `r`.
Przy kilku taskach działających w tle operator nie zauważa, że któryś czeka na niego.

**Brakujący label kończy workflow.** Agent zakończył pracę JSON-em bez klucza `output`, a silnik
oznaczył wykonanie jako `workflow_failed`. Kontrakt wyjścia doklejany do promptu mówi agentowi,
żeby wybrał „jeden z labeli wypisanych wyżej”. Silnik sam tych labeli jednak nie wypisuje, tylko
liczy, że zrobi to autor promptu. W bundlowanych workflow listy labeli z opisami są ręcznie
powielane w każdym prompcie. Jedyną formą odzyskania odpowiedzi jest dziś `retry_policy`, które
uruchamia cały krok od nowa. Tymczasem agent ma w sesji cały kontekst swojej pracy i zwykle
wystarczy mu przypomnieć o formacie.

**Nie ma kogo zapytać o stan taska.** Żeby zdecydować, co zrobić z zatrzymanym albo nieudanym
taskiem, operator sam składa obraz z eventów, rozmów kroków i definicji workflow. Nie ma
asystenta, który ma ten kontekst i mógłby w typowych przypadkach rozstrzygnąć sam albo
przynajmniej przygotować decyzję.

---

## Actors

- A1. Operator: obsługuje TUI, odpowiada na bramki i pytania, rozmawia z dyrygentem.
- A2. Agent kroku: sesja harnessu wykonująca krok workflow. Może nie podać poprawnego labela.
- A3. Silnik narsora: prowadzi workflow, dopytuje agenta o label, wywołuje dyrygenta.
- A4. Dyrygent: asystent przypisany do jednego taska, niezależny od kroków workflow. Działa na
  domyślnym harnessie, tylko do odczytu.

---

## Key Flows

- F1. Ratowanie brakującego labela
  - **Trigger:** Odpowiedź agenta nie zawiera poprawnego labela: brak JSON-a, brak `output` albo
    label spoza zadeklarowanych.
  - **Actors:** A2, A3, A4, A1
  - **Steps:** Silnik wznawia tę samą sesję agenta generycznym dopytaniem z listą dozwolonych
    labeli i ich opisami. Nie zużywa przy tym `retry_policy`. Jeśli odpowiedź nadal jest
    niepoprawna, wywołuje dyrygenta z obiema odpowiedziami agenta. Dyrygent wybiera jedną z
    trzech dróg:
    (a) przy wysokiej pewności sam wybiera label i uzupełnia `data` oraz `variables`;
    (b) raz dopytuje agenta własną treścią, jeśli uzna, że wniesie więcej niż dopytanie silnika;
    (c) eskaluje do operatora jako problem.
    Jeśli po (b) agent nadal nie poda labela, dyrygent nie dopytuje ponownie: wybiera label przy
    wysokiej pewności albo eskaluje.
  - **Outcome:** Workflow przechodzi dalej z pełnym wynikiem kroku albo task czeka na operatora
    jako problem z przygotowaną decyzją. Nie kończy się już `workflow_failed` z powodu samego
    formatu.
  - **Covered by:** R8, R9, R10, R14, R15, R16, R17, R18, R19

- F2. Odpowiedź na bramkę human
  - **Trigger:** Task wchodzi w `input_needed` na bramce `human`.
  - **Actors:** A1, A3
  - **Steps:** Operator widzi licznik czekających tasków, zaznacza task i naciska `i`. Formularz
    pokazuje komunikat bramki, pytanie agenta z poprzedniego kroku oraz pola bramki: wybór opcji
    z opisami i pola tekstowe. Operator wypełnia formularz i wysyła odpowiedź.
  - **Outcome:** Workflow dostaje label i wszystkie wartości pól, a następny krok widzi odpowiedź.
  - **Covered by:** R1, R2, R3, R4, R5, R6, R8, R10, R12

- F3. Błąd kroku
  - **Trigger:** Krok kończy się błędem, który `retry_policy` stanu nie rozwiązało.
  - **Actors:** A3, A4, A1
  - **Steps:** Silnik wywołuje dyrygenta. Jeśli ponowienie dla tego błędu nie zostało jeszcze
    wykorzystane, a nieudana próba nie miała efektów ubocznych, dyrygent ponawia krok raz,
    opcjonalnie z dopiskiem dla agenta. W pozostałych przypadkach pisze w czacie taska diagnozę
    i proponuje akcję do zatwierdzenia.
  - **Outcome:** Operator widzi przyczynę i gotową propozycję, a nie tylko surowy błąd.
  - **Covered by:** R15, R20, R21, R22, R23

- F4. Rozmowa z dyrygentem
  - **Trigger:** Operator otwiera czat dyrygenta w widoku taska i pisze wiadomość.
  - **Actors:** A1, A4
  - **Steps:** Dyrygent odpowiada na podstawie aktualnej migawki taska. Jeśli wiadomość jest
    poleceniem („odpowiedz agentowi, że wariant B”), proponuje konkretną akcję i wykonuje ją
    dopiero po potwierdzeniu przez operatora.
  - **Outcome:** Operator dostaje odpowiedź albo potwierdzoną akcję, a obie zostają w historii
    czatu i w eventach.
  - **Covered by:** R26, R27, R28, R29, R30, R31

---

## Requirements

**Odpowiadanie na input_needed**
- R1. `i` otwiera formularz dla każdego oczekującego inputu: bramki `human`, pytania agenta
  (`awaiting_input`) i eskalowanego problemu (R19). Działa z listy wykonań, z panelu szczegółów i
  z widoku taska.
- R2. Formularz bramki `human` zawiera wszystkie zadeklarowane pola, w tym wybór opcji i pola
  tekstowe, i waliduje wymagalność tak samo jak dziś API.
- R3. Wysłana odpowiedź dociera do workflow z wartościami pól, więc następny krok widzi je w
  kontekście (np. `{{ context.ambiguity_answer }}`).
- R4. `t` pozostaje ścieżką ręcznego przejścia i nadpisania stanu. Przy bramce `human` z
  wymaganymi polami TUI kieruje operatora do `i`, zamiast po cichu przejść dalej bez wartości.
- R5. Jeśli input, na który operator ma otwarty formularz, zostanie zamknięty inną ścieżką
  (dyrygent albo drugie okno TUI), formularz pokazuje, kto i czym odpowiedział, i blokuje
  wysłanie. Wpisana treść zostaje widoczna.

**Treść pytania**
- R6. Każde żądanie inputu, które powstaje po kroku agenta, automatycznie niesie pytanie agenta
  (`data.question`, jeśli jest) oraz końcówkę jego ostatniej odpowiedzi. Dzieje się to bez zmian
  w YAML-u workflow.
- R7. Formularz inputu i szczegóły zdarzenia `human_input_requested` pokazują komunikat bramki
  razem z tym pytaniem i kontekstem, a przy eskalacji od dyrygenta także jego analizę.

**Odświeżanie i sygnał „czeka na Ciebie”**
- R8. Lista wykonań, panel szczegółów i widok taska odświeżają się same co ok. 10 s. Interwał
  można zmienić w konfiguracji, a `r` dalej odświeża natychmiast. Odświeżenie nie zamyka
  otwartego formularza, nie gubi zaznaczenia i nie przesuwa przewiniętego widoku.
- R9. Nagłówek TUI pokazuje, ile tasków czeka na operatora, osobno pytania i problemy (R12).
  Przejście dowolnego taska w jeden z tych stanów pokazuje krótki komunikat w pasku dolnym,
  także gdy zaznaczony jest inny task.

**Pytanie a problem**
- R10. TUI i eventy odróżniają dwa powody, dla których task czeka na operatora:
  - **pytanie**: workflow albo agent zgodnie z planem czeka na decyzję (bramka `human`,
    `awaiting_input`);
  - **problem**: silnik z dyrygentem nie zdołał samodzielnie wyznaczyć przejścia albo dyrygent
    zawiódł.
  Oba różnią się kolorem na liście, opisem w szczegółach i formularzem pod `i`.
- R11. Gdy dyrygent pracuje (R16, R20), lista i widok taska pokazują, że pracuje i nad czym (np.
  „dyrygent: wybór labela po nieudanym dopytaniu”).
- R12. Formularz problemu zawiera wybór labela spośród zadeklarowanych (z opisami), edytowalne
  `data` i `variables` przygotowane przez dyrygenta oraz opcjonalny komentarz. Pokazuje
  odpowiedzi agenta i analizę dyrygenta albo, gdy jej nie ma, surowy błąd.

**Opisy labeli wyjścia**
- R13. Każdy label wyjścia kroku może mieć opcjonalny opis. Workflow z gołymi labelami nadal są
  poprawne. Silnik dokleja do promptu kroku agenta listę dozwolonych labeli, każdy z opisem,
  jeśli opis istnieje. Te same opisy pokazują formularz `i`, menu `t` oraz dopytania silnika i
  dyrygenta. Bundlowane workflow (`workflows/`) przechodzą na opisy, a powielone listy labeli
  znikają z ich promptów. `WORKFLOW_SCHEMA.md` opisuje nowe pole.

**Dopytanie o label**
- R14. Gdy odpowiedź agenta nie zawiera poprawnego labela, silnik raz wznawia tę samą sesję
  agenta dopytaniem z listą dozwolonych labeli, ich opisami i wymaganym formatem. Dotyczy to
  braku JSON-a, braku `output` i labela spoza zadeklarowanych.
- R15. Błędy formatu (R14) nie zużywają `retry_policy` stanu i zawsze idą ścieżką R14–R19.
  `retry_policy` dotyczy tylko awarii akcji. Dyrygent przy błędzie (R20) wchodzi dopiero po
  wyczerpaniu `retry_policy`.
- R16. Po nieudanym dopytaniu silnik nie kończy workflow, tylko przekazuje sprawę dyrygentowi
  razem z obiema odpowiedziami agenta.
- R17. Dyrygent wybiera jedną z trzech dróg:
  (a) sam wybiera label, gdy jego pewność jest wysoka;
  (b) raz dopytuje agenta własną treścią, jeśli uzna, że wniesie więcej niż generyczne dopytanie
      silnika;
  (c) eskaluje do operatora jako problem.
  Po nieudanym (b) dostępne są tylko (a) albo (c). Łącznie agent jest dopytywany najwyżej dwa
  razy: raz przez silnik i raz przez dyrygenta.
- R18. Wybierając label w (a), dyrygent uzupełnia też `data` i `variables` na podstawie
  odpowiedzi agenta, tak jak zrobiłby to agent przestrzegający kontraktu. Dla labeli
  prowadzących do bramki `human` uzupełnia co najmniej `data.question`. Jeśli wartości, których
  potrzebują następne kroki, nie da się ustalić, dyrygent nie wybiera sam, tylko eskaluje.
- R19. Eskalacja przełącza task w stan oczekiwania na operatora jako problem, z formularzem z R12.

**Dyrygent przy błędach**
- R20. Dyrygent jest wywoływany automatycznie przy błędzie akcji, którego nie rozwiązało
  `retry_policy`, oraz przy `workflow_failed`.
- R21. Dla danego błędu dyrygent może sam ponowić krok najwyżej raz, opcjonalnie z dopiskiem dla
  agenta. Wolno mu to tylko wtedy, gdy wiadomo, że nieudana próba nie miała efektów ubocznych
  (np. harness padł, zanim powstała sesja). W pozostałych przypadkach proponuje ponowienie do
  zatwierdzenia. Ponowienie zawsze idzie istniejącą ścieżką ręcznego restartu z notatką.
- R22. Po wykorzystaniu ponowienia albo gdy ponowienie nie jest dozwolone, dyrygent tylko
  diagnozuje i proponuje akcję do zatwierdzenia.
- R23. Dyrygent nigdy sam nie wybiera przejścia po błędzie.

**Awaria dyrygenta**
- R24. Jeśli wywołanie dyrygenta się nie powiedzie albo zostanie przerwane (także przez restart
  daemona), silnik ponawia je raz. Jeśli i to się nie uda, task przechodzi w stan oczekiwania
  na operatora jako problem, z formularzem z R12 i surowym kontekstem (odpowiedzi agenta albo
  błąd), bez analizy dyrygenta.
- R25. Błąd dyrygenta nigdy nie wyzwala R20: dyrygent nie jest wywoływany na własne awarie.

**Dyrygent: zasięg, uprawnienia i czat**
- R26. Każde wykonanie workflow ma własnego dyrygenta i własną historię jego czatu, która
  przetrwa restart TUI i daemona.
- R27. Każde wywołanie dyrygenta, z czatu albo z wyzwalacza, dostaje aktualną migawkę taska:
  definicję workflow, stan i bieżący krok, eventy, rozmowy kroków, oczekujący input oraz
  historię czatu dyrygenta.
- R28. Czat dyrygenta to osobny formularz w widoku taska, niezależny od odpowiedzi na input.
  Dyrygent odpowiada na pytania o stan i historię. Na polecenie proponuje akcję (odpowiedź na
  input, wiadomość do agenta, ponowienie, przejście, przerwanie) i wykonuje ją dopiero po
  potwierdzeniu przez operatora w czacie.
- R29. Dyrygent działa na domyślnym harnessie i tylko do odczytu: nie zmienia plików w
  workspace ani stanu taska inaczej niż przez akcje z R17, R21 i R28.
- R30. Bramki `human` zawsze czekają na operatora. Dyrygent może najwyżej dodać do nich
  podsumowanie i rekomendację, widoczne w formularzu.
- R31. Każda decyzja i akcja dyrygenta trafia do eventów taska razem z uzasadnieniem i
  deklarowaną pewnością. Czat pokazuje ją jako wpis dyrygenta. Operator może ją cofnąć,
  przerywając wykonanie i wymuszając przejście z powrotem do stanu, w którym dyrygent podjął
  decyzję. Efekty uboczne już rozpoczętego kolejnego kroku nie są cofane.

---

## Acceptance Examples

- AE1. **Covers R1, R2, R3.** Mamy task w `resolve_ambiguity`. Operator naciska `i`, wybiera
  `answered` i wpisuje „używamy wariantu B”. Wtedy `apply_clarification` dostaje w prompcie
  „używamy wariantu B”.
- AE2. **Covers R6, R7.** Krok `work` zwrócił `needs_human_decision` z `data.question` =
  „Czy limit ma dotyczyć konta czy użytkownika?”. Formularz `i` przy `resolve_ambiguity`
  pokazuje to pytanie nad polami, a workflow YAML pozostaje bez zmian.
- AE3. **Covers R14, R16, R17, R18.** Agent zakończył JSON-em bez `output`, a po dopytaniu silnika
  odpowiedział „gotowe, wszystko działa” bez JSON-a. Dyrygent widzi w odpowiedziach przechodzące
  testy i ukończone jednostki. Z wysoką pewnością wybiera `implemented`, uzupełnia
  `data.units_completed`, a workflow przechodzi do `simplify`. Event zawiera uzasadnienie.
- AE4. **Covers R10, R12, R17, R19.** Po dopytaniu silnika odpowiedź nadal jest niejednoznaczna,
  a dyrygent uznaje, że własne dopytanie nic nie wniesie. Task pojawia się na liście jako
  problem (innym kolorem niż pytanie). Operator widzi obie odpowiedzi agenta, analizę dyrygenta
  i wybór spośród `implemented` oraz `needs_human_decision` z opisami.
- AE5. **Covers R17.** Dyrygent dopytał agenta własną treścią, a agent znów nie podał labela.
  Dyrygent nie dopytuje trzeci raz: wybiera label przy wysokiej pewności albo eskaluje.
- AE6. **Covers R18.** Dyrygent skłania się ku `needs_human_decision`, ale z odpowiedzi agenta nie
  da się ustalić, jakie pytanie zostało otwarte. Nie wybiera sam: eskaluje, a formularz problemu
  pozwala operatorowi wpisać `data.question`.
- AE7. **Covers R15.** Stan ma `retry_policy` z `max_attempts: 2`, a agent zwraca JSON bez `output`.
  Krok nie rusza od nowa: silnik dopytuje w tej samej sesji, a licznik prób `retry_policy` się
  nie zmienia.
- AE8. **Covers R20, R21, R22, R23.** Harness padł przy starcie, zanim powstała sesja, i
  `retry_policy` się wyczerpało. Dyrygent sam ponawia krok raz. Po drugiej takiej samej awarii
  pisze w czacie diagnozę i proponuje przejście, ale go nie wykonuje.
- AE9. **Covers R21.** Krok `script` padł w połowie po zmianie plików w workspace. Dyrygent nie
  ponawia go sam, tylko proponuje ponowienie z notatką do zatwierdzenia.
- AE10. **Covers R24, R25.** Wywołanie dyrygenta kończy się błędem autoryzacji harnessu. Silnik
  ponawia je raz, a przy drugim błędzie task staje się problemem z surowymi odpowiedziami agenta.
  Błąd dyrygenta nie uruchamia kolejnego dyrygenta.
- AE11. **Covers R8.** Operator ma otwarty formularz `i`, gdy mija 10 s. Formularz zostaje otwarty
  z wpisaną treścią, a status listy w tle się aktualizuje.
- AE12. **Covers R9, R11.** Operator ogląda task A, a task B przechodzi w problem. Nagłówek pokazuje
  „1 problem”, a pasek dolny krótki komunikat o B. Wcześniej, gdy dyrygent analizował B, lista
  pokazywała przy B, że dyrygent pracuje.
- AE13. **Covers R5.** Operator ma otwarty formularz z „wariant A”, a w tym czasie zatwierdzona w
  czacie akcja dyrygenta odpowiada na to samo pytanie „wariant B”. Formularz pokazuje „odpowiedział
  już dyrygent: wariant B”, blokuje wysłanie i zachowuje wpisane „wariant A”.
- AE14. **Covers R4.** Na `resolve_ambiguity` operator naciska `t` i wybiera `answered`. TUI nie
  przechodzi dalej bez `ambiguity_answer`, tylko wskazuje, że tę bramkę wypełnia się pod `i`.
- AE15. **Covers R13.** Label `needs_human_decision` ma opis w YAML-u. Ten sam opis widać w liście
  labeli doklejonej do promptu kroku, w dopytaniu silnika, w formularzu problemu i w menu `t`.
  Prompt kroku w bundlowanym `compound-engineering.yaml` nie zawiera już ręcznej listy labeli.
- AE16. **Covers R30.** Task wchodzi w bramkę akceptacji planu. Dyrygent dodaje do formularza
  podsumowanie planu i rekomendację „approved”, ale bramka czeka na operatora i nic nie przechodzi
  samo.
- AE17. **Covers R28, R31.** Operator pisze w czacie dyrygenta „odpowiedz agentowi, że wariant B”.
  Dyrygent proponuje konkretną odpowiedź na oczekujące pytanie i wysyła ją dopiero po
  potwierdzeniu operatora. Wysłanie trafia do eventów.

---

## Success Criteria

- Operator odpowiada na każdą bramkę i pytanie agenta bez wychodzenia z TUI i bez szukania
  pytania w danych wykonania.
- Status tasków w TUI jest aktualny bez naciskania `r`, a task czekający na operatora jest
  zauważalny, także gdy nie jest zaznaczony.
- Operator od razu odróżnia task czekający na zaplanowaną decyzję od taska z problemem.
- Sama wadliwa forma odpowiedzi agenta nie kończy już workflow `workflow_failed`. Kończy się
  wybranym labelem z pełnym wynikiem kroku albo przygotowaną eskalacją.
- Każdą decyzję dyrygenta da się odtworzyć z eventów: co zrobił, dlaczego i z jaką pewnością.
- Planowanie nie musi dopowiadać, kiedy dyrygent działa sam, kiedy pyta, a kiedy tylko
  proponuje. Te granice wyznaczają R17, R18, R21, R23, R28 i R30.

---

## Scope Boundaries

- Uprawnienie nadawane w kroku YAML, które pozwala dyrygentowi samemu wybrać przejście po błędzie.
  To pomysł na przyszłość; w tej iteracji przejścia po błędzie zawsze zatwierdza operator.
- Globalny dyrygent obejmujący wiele tasków (np. „co utknęło?”).
- Automatyczne odpowiadanie przez dyrygenta na pytania agenta (`awaiting_input`) i na bramki
  `human`.
- Zmiany w kodzie workspace wykonywane przez dyrygenta.
- Konfigurowalna liczba dopytań i ponowień dyrygenta. Limity są stałe: jedno dopytanie silnika,
  jedno dopytanie dyrygenta, jedno ponowienie kroku na błąd, jedna powtórka wywołania
  dyrygenta. Istniejące `retry_policy` stanów zostaje bez zmian.
- Prompt dyrygenta dostosowywany per workflow.
- Utrzymywanie żywej, dwukierunkowej sesji harnessu na czas dopytań. W tej iteracji silnik
  kończy proces i wznawia sesję (patrz Outstanding Questions).

---

## Key Decisions

- Dyrygent jako świeże wywołanie z migawką, a nie długa sesja harnessu. Decyzje opierają się na
  aktualnym stanie, a nie na skompaktowanej pamięci sesji. Rozwiązanie nie zależy od
  wznawiania sesji w konkretnym harnessie i łatwo je odtworzyć po crashu. Ceną jest droższe
  pojedyncze wywołanie.
- Jeden dyrygent na task: zasięg i uprawnienia pozostają jasne, a migawka ograniczona.
- Pytanie agenta dołącza silnik, a nie szablon YAML: działa w każdym workflow bez migracji.
- Dopytanie o label w tej samej sesji: agent zachowuje kontekst swojej pracy, a tania korekta
  formatu nie wymaga ponownego wykonania kroku. Dlatego błędy formatu nie zużywają
  `retry_policy`, które uruchamia krok od nowa.
- Kolejność mechanizmów: format → dopytanie silnika → dyrygent; awaria → `retry_policy` →
  dyrygent. Każdy mechanizm obsługuje jedną klasę problemu, więc limity są rzeczywiste.
- Limit dwóch dopytań łącznie. Agent, który nie poprawił się ani po dopytaniu systemu, ani po
  dopytaniu dyrygenta, nie poprawi się po kolejnym.
- Dyrygent decyduje sam tylko przy wysokiej pewności, a operacje z czatu wymagają
  potwierdzenia. Autonomia jest więc tam, gdzie błąd jest tani (wybór labela, jedno
  ponowienie), a kontrola zostaje u operatora tam, gdzie zmienia się przebieg workflow.
- Automatyczne ponowienie tylko dla prób bez efektów ubocznych. Uzasadnienie: zasada D23/D24 i
  commit `a746f68` celowo wykluczają automatyczny replay, bo krok, który padł w połowie, mógł
  zmienić workspace. Samodzielne ponowienie dyrygenta jest świadomym, wąskim wyjątkiem i korzysta
  z tej samej ścieżki restartu z notatką co operator.
- Rozróżnienie „pytanie” i „problem”: pytanie to zaplanowana część przebiegu, problem to sytuacja
  awaryjna. Wymagają innej uwagi i innego formularza, więc nie mogą wyglądać tak samo.
- Opisy labeli jako jedno źródło prawdy. Kontrakt wyjścia, dopytania, dyrygent i TUI korzystają
  z tych samych opisów, co usuwa ręczne powielanie list w promptach.

---

## Dependencies / Assumptions

- Adaptery claude-code, codex i opencode potrafią wznowić sesję (`resume_session`), ale silnik
  robi to dziś tylko na jawne żądanie z YAML-a. Ścieżka `awaiting_input` uruchamia nową sesję z
  odpowiedzią doklejoną do promptu. R14 wymaga nowej zdolności silnika: wznowienia sesji kroku
  po przechwyconym `harness_session_id`, na harnessie, który faktycznie wykonał krok, także
  domyślnym. Harness, który nie zwrócił identyfikatora sesji, pomija dopytanie silnika i od
  razu oddaje sprawę dyrygentowi.
- „Wysoka pewność” to jawna deklaracja dyrygenta z uzasadnieniem. Brak deklaracji traktujemy jak
  niską pewność.
- Wykonanie po `workflow_failed` da się ponowić istniejącym ręcznym restartem z notatką
  operatora (commit `a746f68`) i przejść ręcznie. Ponowienie z R21 korzysta z tej ścieżki.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R10, R11, R19][Technical] Czy „problem” i „dyrygent pracuje” to osobne statusy
  wykonania, czy odmiany istniejących (`input_needed`, `running`)? Nowy status dotyka
  rekoncyliacji, API i TUI.
- [Affects R14][Needs research] Czy zamiast kończyć proces i wznawiać sesję, silnik może
  utrzymać żywą, dwukierunkową sesję harnessu (claude w trybie strumieniowym, codex w trybie
  serwera, opencode jako serwer)? Warto wrócić do tego, jeśli dopytań będzie dużo albo
  dyrygent zacznie prowadzić z agentem dłuższe rozmowy. Trzeba wtedy rozstrzygnąć, co przy
  restarcie daemona z żywym procesem.
- [Affects R29][Needs research] Jak wymusić tryb tylko do odczytu dyrygenta na każdym harnessie?
  Istniejące `workspace_access: read-only` działa dla claude (tryb `plan`) i codex
  (`--sandbox read-only`), ale opencode nie ma flagi wymuszającej, a workspace taska musi
  pozostać zapisywalny. Alternatywa: dyrygent bez dostępu do workspace, w pustym katalogu, z
  kontekstem wyłącznie z migawki i akcjami zwracanymi jako strukturalna odpowiedź. Koszt:
  dyrygent nie przeczyta kodu ani planu, jeśli nie trafią do migawki.
- [Affects R14, R16][Technical] Czy nieudane dopytanie ma być zapisane w eventach jako osobne
  zdarzenia (dopytanie, odpowiedź), czy jako część próby kroku?
- [Affects R21][Technical] Jak rozpoznać „ten sam błąd” i „brak efektów ubocznych” przy liczeniu
  jednego ponowienia: po kroku, po klasie błędu czy po próbie?
- [Affects R26, R27][Technical] Jak ograniczyć migawkę przy długich taskach (rozmowy kroków bywają
  duże), żeby zmieściła się w kontekście modelu?
- [Affects R28][Technical] Co robi dyrygent, gdy operator pisze w czacie w trakcie jego
  automatycznego wywołania? Kolejkować czy odrzucać?
- [Affects R8][Needs research] Które elementy da już strumień eventów daemona, a które wymagają
  okresowego odczytu? Chodzi o to, żeby odświeżanie nie dublowało strumienia.
