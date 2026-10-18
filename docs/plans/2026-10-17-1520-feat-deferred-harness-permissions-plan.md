---
title: Odroczone decyzje o uprawnieniach harnessu - Plan
type: feat
date: 2026-10-17
topic: deferred-harness-permissions
artifact_contract: ce-unified-plan/v1
artifact_readiness: implementation-ready
deepened: 2026-10-17
product_contract_source: ce-brainstorm
execution: code
---

# Odroczone decyzje o uprawnieniach harnessu - Plan

## Goal Capsule

- **Objective:** Task Narsora może zatrzymać się na prośbie Claude Code o uprawnienie, poczekać na późniejszą decyzję użytkownika i kontynuować bez interaktywnego terminala.
- **Means:** Narsor zapisuje prośbę jako trwały stan oczekiwania, a zgodę przekazuje wyłącznie do wznowienia danego taska. Profil harnessu ma katalog źródłowy i katalog roboczy. Gdy są różne, Narsor przed startem harnessu kopiuje albo scala profil do katalogu roboczego.
- **Product authority:** Zgody na polecenia są lokalne dla taska i wygasają wraz z nim. Runner jest tylko konsumentem profilu: zmiany harnessu zostają lokalne i nie są dystrybuowane. Profil w Secrecie zmienia tylko kontroler Narsor (sesja edycji) albo operator poza Narsorem. Przy zapisie ostatni zapis wygrywa.
- **Open blockers:** Brak blokujących decyzji dla instalacji lokalnej. Szczegóły K8s (transport terminala edycji, format Secretu, reguły scalania wg typu pliku) są wstępne i zostaną dopracowane podczas praktycznych testów w K8s.

---

## Product Contract

### Summary

Plan realizuje trwałe, taskowe decyzje o uprawnieniach Claude Code przez istniejące kanały Narsora, zachowanie znaczenia pominiętego `workspace_access` oraz wspólny model profilu harnessu: katalog źródłowy i katalog roboczy. W instalacji lokalnej harness używa swojego standardowego katalogu bez kopiowania. W K8s źródłem jest Secret zawierający pliki potrzebne harnessowi, w tym uwierzytelnianie, ustawienia uprawnień, MCP, pluginy i skills; edycję Secretu wykonuje kontroler Narsor.

### Problem Frame

Obecny adapter Claude Code potrafi zapisać `permission_denied` jako zdarzenie techniczne, ale Narsor nie zamienia go w trwałą prośbę do użytkownika. Runner uruchamia proces bez stdin, więc standardowy interaktywny permission prompt nie pasuje do autonomicznego taska. Log `docs/sources/permissions.log` zawiera identyfikator wywołania narzędzia oraz polecenie, co daje podstawę do powiązania odmowy z konkretną akcją.

Użytkownik może odpowiedzieć później, dlatego task nie może traktować braku odpowiedzi jak odmowy ani kończyć się nieczytelnym błędem. Jednocześnie decyzja o jednym poleceniu nie może stać się trwałym uprawnieniem dla kolejnych tasków.

W lokalnym runnerze Claude może korzystać z konfiguracji lokalnego środowiska; tam zmiany konfiguracji mogą być wykonywane w dowolnym momencie przez wszystkie instancje harnessu. W K8s nie ma dostępu do konfiguracji z komputera operatora: domyślny `HOME` jest prywatny dla taska, a wolumen Secretu jest tylko do odczytu. Potrzebny jest zatem profil źródłowy dostarczany podom runnerów, kopiowany do zapisywalnego katalogu roboczego, oraz osobna sesja edycji, która zapisuje zmiany z powrotem do Secretu.

### Key Decisions

- **Asynchroniczna obsługa uprawnień.** (session-settled: user-directed — decyzja użytkownika może nadejść po zatrzymaniu taska.) Governs R1-R8.
- **Jedno ustawienie workflow.** (session-settled: user-directed — używamy istniejącego `workspace_access`, bez dodatkowego pola trybu uprawnień.) Governs R9-R10.
- **Dwa parametry profilu: katalog źródłowy i katalog roboczy.** (session-settled: user-directed — gdy źródło jest puste albo równe katalogowi roboczemu, Narsor nie kopiuje i tylko uruchamia harness; tak działa instalacja lokalna.) Governs R11-R12.
- **Runner jest tylko konsumentem profilu i scala go przy każdym starcie.** (session-settled: user-directed — Secret może nieść np. nową sesję OAuth, więc musi trafić do harnessu; lokalne zmiany harnessu są wykrywane i nanoszone na wersję z Secretu, ale nigdzie nie są dystrybuowane.) Governs R12-R14.
- **Edycję i zapis Secretu wykonuje wyłącznie kontroler Narsor; ostatni zapis wygrywa.** (session-settled: user-directed — kontroler kopiuje profil, uruchamia harness dla użytkownika, po jego zamknięciu zbiera pliki i zapisuje Secret; preferowany przebieg w podzie kontrolera.) Governs R15-R17.
- **Pierwszy zakres obejmuje Claude Code.** (inferred — rzeczywisty log i obecny parser dotyczą tego harnessu; inne adaptery można rozszerzyć, gdy dostarczą równoważny sygnał.) Governs R1, R17.

### Actors

- A1. **Właściciel taska** przegląda oczekujące prośby i wybiera zakres zgody; przy edycji profilu sam uruchamia Claude Code i zmienia konfigurację.
- A2. **Runner Narsora** obserwuje zdarzenia harnessu, zapisuje oczekiwanie, wstrzymuje task i wznawia go z lokalną decyzją; przed startem harnessu przygotowuje katalog roboczy profilu.
- A3. **Claude Code** wykonuje narzędzia zgodnie ze swoją konfiguracją oraz zgodami przekazanymi dla bieżącego taska.
- A4. **Kontroler Narsor** ma zamontowany Secret profilu i wyłączne prawo jego zapisu; prowadzi sesję edycji: kopiuje profil, uruchamia harness, zbiera pliki i zapisuje Secret.
- A5. **Pod runnera** wykonuje każde zadanie Claude Code w K8s i otrzymuje dedykowany Secret profilu tylko do odczytu.

### Requirements

**Oczekiwanie i zgoda taskowa**

- R1. Gdy Claude Code zgłosi zdarzenie uprawnienia możliwe do rozstrzygnięcia przez użytkownika, Narsor tworzy trwałe oczekiwanie typu `permission`, odrębne od zwykłego pytania agenta i od błędu wykonania.
- R2. Prośba pokazuje nazwę narzędzia, dokładne polecenie lub akcję, katalog roboczy, powód odmowy, dostępne źródło decyzji oraz powiązanie z wywołaniem narzędzia i sesją.
- R3. Użytkownik może odmówić, zezwolić na ponowienie konkretnego polecenia albo zezwolić na podobne polecenia określonego typu do końca taska. Interfejs jasno pokazuje wybrany zakres.
- R4. Po odmowie task może wznowić agenta z informacją o decyzji, aby agent mógł wybrać inną drogę lub zakończyć pracę.
- R5. Po zgodzie Narsor wznawia tę samą logiczną sesję i przekazuje grant wyłącznie temu taskowi. Grant nie zmienia plików konfiguracji Claude Code, ustawień użytkownika ani konfiguracji Secretu.
- R6. Jawna reguła `deny` lub odmowa wynikająca z polityki, której użytkownik nie może nadpisać lokalną zgodą, nie jest przedstawiana jako prośba możliwa do zatwierdzenia. Narsor pokazuje ją jako blokadę z wyjaśnieniem.
- R7. Stan oczekiwania i decyzja pozostają trwałe po zatrzymaniu runnera, utracie klienta i odtworzeniu poda. Zgoda wygasa po zakończeniu lub usunięciu taska.
- R8. Oczekiwanie jest dostępne przez istniejące kanały Narsora obsługujące decyzje użytkownika; odpowiedź nie wymaga otwartego terminala ani żywego stdin procesu harnessu.
- R9. W workflow pozostaje wyłącznie `workspace_access`. Jawne `read-only` ogranicza dostęp, jawne `read-write` wybiera tryb zapisu danego adaptera, a pominięcie pola nie wymusza trybu i pozostawia decyzję konfiguracji harnessu dostępnej w środowisku.
- R10. Schemat workflow zachowuje informację, czy `workspace_access` podano jawnie, aby wartość domyślna nie wymuszała niejawnie trybu harnessu. Istniejące workflow otrzymują zdefiniowaną, kompatybilną interpretację.

**Profil harnessu (lokalnie i w Kubernetesie)**

- R11. Konfiguracja Narsora ma dwa parametry profilu harnessu: katalog źródłowy i katalog roboczy. Gdy katalog źródłowy jest pusty albo wskazuje ten sam katalog co roboczy, Narsor nie kopiuje profilu i tylko uruchamia harness z katalogiem roboczym (lub standardowym katalogiem harnessu). Katalog roboczy jest wymagany, gdy podano katalog źródłowy albo gdy jest niestandardowy.
- R12. Gdy katalogi są różne, runner przed każdym startem harnessu przygotowuje katalog roboczy z katalogu źródłowego i wskazuje go harnessowi (`CLAUDE_CONFIG_DIR`). W K8s katalogiem źródłowym jest wolumen dedykowanego Secretu tylko do odczytu, montowany w każdym podzie Claude Code.
- R13. Przy ponownym starcie runner scala profil: zmiany wykonane lokalnie przez harness od poprzedniego startu są nanoszone na aktualną wersję ze źródła. Nowa wartość ze źródła (np. sesja OAuth) zawsze trafia do harnessu. Pliki spoza profilu (sesje, historia) pozostają bez zmian.
- R14. Lokalne zmiany profilu w runnerze nie są nigdzie dystrybuowane. Runner nie ma prawa zapisu Secretu.
- R15. Sesja edycji profilu działa w kontrolerze Narsor: kontroler kopiuje profil do katalogu zapisywalnego, uruchamia harness dla użytkownika, a po zamknięciu harnessu zbiera pliki profilu z katalogu roboczego i zapisuje je do Secretu. Harness nie zapisuje bezpośrednio do Secretu i nie dostaje uprawnień Kubernetes.
- R16. Zewnętrzna zmiana Secretu jest przyjmowana bez weta Narsora. Przy równoległej zmianie zewnętrznej i zapisie sesji edycji ostatni zapis wygrywa.
- R17. Do Secretu można zapisać co najmniej uwierzytelnianie, ustawienia i reguły uprawnień, MCP, pluginy oraz skills. Historia sesji i dane konkretnego taska nie są częścią współdzielonego profilu.
- R18. Obecne zachowanie innych harnessów pozostaje bez zmian, dopóki ich adaptery nie obsłużą własnego źródła i cyklu życia próśb o uprawnienia.

### Key Flows

- F1. **Odmowa harnessu:** Claude Code emituje odmowę powiązaną z wywołaniem narzędzia; Narsor zapisuje prośbę, zatrzymuje task i pokazuje polecenie oraz przyczynę. **Covers:** R1-R2.
- F2. **Zgoda na polecenie:** Użytkownik zatwierdza konkretne polecenie; Narsor zapisuje grant w stanie taska i wznawia sesję. **Covers:** R3, R5, R7.
- F3. **Zgoda na grupę:** Zgodne akcje tego taska nie tworzą ponownie tej samej prośby, ale inny task wymaga własnej decyzji. **Covers:** R3, R5, R7.
- F4. **Odmowa użytkownika:** Agent wznawia pracę z informacją o decyzji i może dobrać alternatywę. **Covers:** R4.
- F5. **Blokada polityki:** Narsor pokazuje nieprzekraczalną regułę `deny` bez opcji jej obejścia. **Covers:** R6.
- F6. **Start lokalny:** Katalog źródłowy jest pusty albo równy roboczemu; runner uruchamia harness bez kopiowania. **Covers:** R11.
- F7. **Start i wznowienie taska K8s:** Runner scala profil z Secretu z lokalnymi zmianami harnessu w prywatnym katalogu roboczym, potem uruchamia Claude Code. Sesja i oczekiwanie są zachowane. **Covers:** R7, R12-R14.
- F8. **Edycja profilu:** Kontroler kopiuje zamontowany profil do katalogu zapisywalnego, uruchamia Claude Code dla użytkownika; użytkownik zmienia permissions, pluginy, skills, MCP i zamyka harness; kontroler zbiera pliki profilu i zapisuje je do Secretu. **Covers:** R15-R17.

### Acceptance Examples

- AE1. Covers R1-R3. Po odmowie Bash z dokładnym poleceniem `cargo build`, użytkownik widzi przyczynę i wybiera zgodę tylko dla tego polecenia; prośba nie jest klasyfikowana jako zwykłe pytanie agenta.
- AE2. Covers R3, R5, R7. Zgoda na grupę poleceń działa w pozostałej części taska i znika po jego zakończeniu; drugi task nadal zatrzymuje się przy analogicznej odmowie.
- AE3. Covers R4, R6. Po odmowie użytkownika agent może kontynuować inną ścieżką, a jawna polityka `deny` nie może zostać nadpisana przez lokalną zgodę.
- AE4. Covers R8-R10. Klient może odpowiedzieć na oczekiwanie, gdy proces runnera jest zatrzymany lub został odtworzony; workflow bez jawnego `workspace_access` nie wysyła harnessowi wymuszonego trybu, a jawne `read-only` nadal ogranicza zapis.
- AE5. Covers R11-R14. Lokalnie przy pustym źródle runner nie kopiuje profilu. W K8s nowy pod kopiuje profil do katalogu roboczego; wznowiony pod otrzymuje nowy token OAuth z Secretu i zachowuje lokalnie dodaną regułę uprawnień.
- AE6. Covers R15-R17. Sesja edycji w kontrolerze zapisuje zmienione pliki profilu do Secretu; historia sesji nie trafia do Secretu; runner nie ma prawa zapisu Secretu.
- AE7. Covers R18. Harness inny niż Claude Code zachowuje dotychczasowy przebieg i nie otrzymuje nowego zachowania przez przypadkowe współdzielenie parsera zdarzeń.

### Scope Boundaries

- Funkcja nie wprowadza interaktywnego terminala ani nie czeka na stdin procesu Claude Code.
- Zgoda na polecenie nie zmienia konfiguracji Claude Code ani nie nadaje podowi nowych uprawnień infrastrukturalnych. (R5-R6)
- Zmiany profilu w runnerze są lokalne; nie ma ich publikacji z runnera. Zewnętrzna aktualizacja Secretu nie czeka na akceptację Narsora. (R14, R16)
- Pierwsza dostawa nie musi zamknąć szczegółów K8s: transport terminala edycji (`kubectl exec` albo terminal po HTTPS), format Secretu i reguły scalania wg typu pliku zostaną dopracowane podczas testów w K8s. (R12-R17)
- Zakres nie zmienia domyślnego zachowania innych harnessów. (R18)

---

## Planning Contract

Product Contract zaktualizowany 2026-10-18: runner tylko konsumuje profil, edycję i zapis Secretu wykonuje kontroler, ostatni zapis wygrywa.

### High-Level Technical Design

```mermaid
sequenceDiagram
    participant U as Użytkownik / klient Narsora
    participant N as Narsor API i stan taska
    participant R as Runner taska
    participant C as Claude Code
    participant Ktrl as Kontroler Narsora
    participant K as Kubernetes Secret

    R->>C: uruchom print mode z obsługą odmów
    C-->>R: permission_denied + dane wywołania
    R->>N: zapisz typowane oczekiwanie permission
    U->>N: zaakceptuj zakres albo odmów
    N->>R: dostarcz jednorazową decyzję dla taska
    R->>C: wznów tę samą sesję z grantem w argumentach

    K-->>R: profil (wolumen read-only)
    R->>R: scal profil z lokalnymi zmianami w katalogu roboczym

    Ktrl->>K: odczytaj zamontowany profil
    Ktrl->>Ktrl: skopiuj profil do katalogu zapisywalnego
    Ktrl->>C: uruchom harness dla użytkownika
    U->>C: zmień permissions, pluginy, skills, MCP; zamknij harness
    Ktrl->>Ktrl: zbierz pliki profilu z katalogu roboczego
    Ktrl->>K: zapisz Secret (ostatni zapis wygrywa)
```

`permission_denied` staje się oczekiwaniem dopiero po zakończeniu nieinteraktywnego wywołania Claude Code. Narsor zapisuje request razem z sesją i identyfikatorem wywołania, a dedykowana decyzja właściciela albo odmawia, albo dodaje ograniczony grant do wznowienia. Zwykłe odpowiedzi na pytania agenta i ogólne polecenie wznowienia nie mogą tworzyć grantów.

Profil ma trzy odrębne warstwy: katalog źródłowy (w K8s dedykowany Secret), zapisywalny katalog roboczy harnessu oraz granty przechowywane w stanie taska. Runner przygotowuje katalog roboczy przed każdym startem harnessu i nic nie publikuje. Jedyną drogą zapisu Secretu w Narsorze jest sesja edycji w kontrolerze z uprawnieniami ograniczonymi do nazwanego Secretu.

### Key Technical Decisions

- KTD1. **Użyj jawnego trybu odmów Claude Code w nieinteraktywnym print mode.** Uruchamiaj z `--permission-prompts none` i klasyfikuj strukturalne `system/permission_denied` oraz `result.permission_denials`. Flaga wymaga CLI v2.1.259+; dla runtime przyjmij wspieraną i nagraną wersję co najmniej 2.1.302. W obecnym wspieranym `-p` `AskUserQuestion` jest niedostępne już bez tej flagi, więc nie obiecuj ścieżki narzędziowej: zachowaj testy syntetycznego requestu `tool_use` i tekstowego fallbacku agenta, a nagranie realnego CLI ma potwierdzić strukturalną odmowę oraz brak retry. Zmiana tego kontraktu w nowszej wersji wymaga aktualizacji fixture i testów przed dopuszczeniem jej do runtime. (see [Claude Code headless mode](https://code.claude.com/docs/en/headless), [CLI reference](https://code.claude.com/docs/en/cli-reference))
- KTD2. **Rozszerz trwałe oczekiwanie człowieka o typowaną decyzję permission.** Reuse pliku i statusu `InputNeeded` z `human.rs`, ale dodaj oddzielną kontynuację i operację `resolve_permission`; zwykły endpoint `/input` nie może obsłużyć tej kontynuacji. Zapisz oczekiwanie przed zmianą statusu, waliduj identyfikator requestu i atomowo zamykaj je po jednej decyzji. (session-settled: user-directed — wybrane zamiast interaktywnego promptu stdin, ponieważ odpowiedź może przyjść po zatrzymaniu taska; governs R1-R8)
- KTD3. **Przekazuj zgodę tylko w kontekście bieżącego taska i requestu.** Decyzja ma zamknięty typ `deny` albo `allow`; `allow` wymaga jawnego zakresu `exact_operation`, `command_group` albo `pattern`. Decyzja bez `denial_index` dotyczy wszystkich odmów requestu naraz. Grant jest listą reguł `--allowedTools`, zapisaną w stanie taska i jednorazową dla wznowienia requestu. Claude Code ocenia polecenie złożone osobno dla każdego członu, a reguła z pełnym tekstem polecenia złożonego nic nie przepuszcza (sprawdzone na CLI 2.1.302). Dlatego polecenie Bash jest dzielone na człony po `&&`, `||`, `;` i `|`, przekierowania do deskryptora lub `/dev/null` są pomijane, a `exact_operation` daje regułę `Bash(<człon>)` dla każdego członu. `command_group` daje regułę `Bash(<program> <podkomenda> *)` dla każdego członu, ale tylko dla par program–podkomenda i narzędzi z jawnej listy dozwolonych (`crates/narsor-protocol/src/api/permission_rules.rs`); każde inne polecenie dostaje tylko `exact_operation`. Odrzucaj zgodę dla substitution, zmiennych, escape, globów, przekierowania do pliku, `<`, `&` w tle, nawiasów, komentarzy oraz metaznaków wzorca reguły i znaków sterujących także w cudzysłowach. Zakres `pattern` daje jedną regułę `Bash(<wzorzec>)` z wzorca wpisanego przez właściciela (całe polecenie albo początek z `*`); wzorzec nie jest zawężany, odrzucane są tylko pusty wzorzec, nawiasy i znaki sterujące, a sam `*` zezwala na każde polecenie Bash do końca taska i interfejs wyraźnie to pokazuje. Polecenie, którego żadna reguła nie opisze dokładnie (zmienne, pętle), przechodzi tylko przy `Bash(*)` — reguła z prefiksem, np. `Bash(until *)`, nic nie przepuszcza (sprawdzone na CLI 2.1.302 w `acceptEdits`). Odmowa takiego polecenia prosi agenta o proste polecenia, a każda decyzja może nieść wiadomość właściciela dla agenta. Zgodę można dać tylko dla odmów z `decision_reason_type` `asyncAgent` lub `subcommandResults`; pozostałe powody są blokadą. Nie zapisuj grantów do settings ani bazowego Secretu. Jawne reguły `deny` pozostają nadrzędne wobec allow rules. (session-settled: user-directed — zgoda na polecenia złożone po członach i lista dozwolonych grup ustalone 2026-10-18 po teście w prawdziwym CLI; wzorzec wpisany przez właściciela, z `*` dopuszczonym z ostrzeżeniem, ustalony 2026-10-18; governs R3-R7)
- KTD4. **Zachowaj obecne powierzchnie decyzji i daj im wspólny typ requestu.** Każdy endpoint autoryzuje właściciela względem taska, a request permission musi należeć do tego taska. API, CLI, TUI i kontroler tasków używają tej samej struktury i walidacji; stare `/input` i `/resume` nie mogą ominąć decyzji permission. Odpowiedź wiąże się z ID oczekiwania i ma pojedynczego zwycięzcę przy równoczesnych zgłoszeniach. (governs R2-R8)
- KTD5. **Zachowaj obecność pola workflow osobno od wartości.** Nowy workflow z pominiętym `workspace_access` nie przekazuje jawnego trybu do adaptera; jawne `read-only` i `read-write` nadal są mapowane. Rekordy wykonania zapisane przed zmianą zachowują dotychczasowe znaczenie `read-write`. (session-settled: user-directed — wybrane zamiast nowego pola trybu uprawnień, ponieważ workflow ma nadal jedno ustawienie dostępu; governs R9-R10)
- KTD6. **Scalanie trójstronne profilu w runnerze.** Przy każdym starcie runner porównuje dla każdego pliku profilu trzy wersje: bazę (kopię źródła z poprzedniego startu, w bookkeepingu taska), wersję lokalną (katalog roboczy) i aktualne źródło. Plik niezmieniony lokalnie dostaje wersję ze źródła; plik niezmieniony w źródle zachowuje wersję lokalną; nowa baza jest zapisywana po scaleniu. Pliki, które zmieniły się po obu stronach, rozstrzyga typ pliku (wstępnie, do weryfikacji w K8s): plik poświadczeń (`.credentials.json`) bierze w całości wersję ze źródła, bo tokeny z różnych sesji nie mogą być mieszane; pliki ustawień JSON (`settings.json`, `.claude.json`) są scalane per klucz, listy reguł uprawnień są sumowane, a konflikt tej samej wartości rozstrzyga źródło; pozostałe pliki (skills, `CLAUDE.md`) biorą wersję ze źródła. Gdy źródło jest puste albo równe katalogowi roboczemu, krok nie działa. Wpisy wewnętrzne wolumenu Secretu (`..data`, `..<timestamp>`) nie są częścią profilu. (session-settled: user-directed — governs R11-R14)
- KTD7. **Edycja profilu w kontrolerze, ostatni zapis wygrywa.** Rdzeń edycji (kopiuj → uruchom harness → zbierz → zapisz) jest niezależny od transportu terminala; pierwszy transport to komenda kontrolera uruchamiana przez `kubectl exec -it`, terminal po HTTPS można dodać później. Zbierane są tylko pliki profilu wg reguł include/exclude (domyślnie: ustawienia, poświadczenia, `CLAUDE.md`, skills, agents, commands, rejestry pluginów; zawsze bez `projects/`, historii, todos, snapshotów powłoki i cache pluginów). Zapis nie używa CAS. RBAC daje zapis Secretu tylko kontrolerowi. Stary przepływ „propozycja → akceptacja” jest usuwany. Format Secretu (mapowania `klucz=ścieżka` albo jeden klucz z archiwum profilu) pozostaje otwarty do testów w K8s: mapowania wymagają wpisu konfiguracji dla każdego nowego pliku, archiwum nie. (session-settled: user-directed — governs R15-R17)

### Risks and Mitigations

- **Niewspierany lub zmieniony protokół CLI.** Utrzymuj zarejestrowane przykłady prawdziwego CLI dla dozwolonej odmowy, blokady polityki i wznowienia. Oddziel test syntetycznego `AskUserQuestion` od realnego tekstowego fallbacku w `-p`; przed wdrożeniem potwierdź korelację, brak ponawiania odmowy oraz skuteczność ograniczonego grantowania. Przy braku rozstrzygających danych nie pokazuj opcji zatwierdzenia.
- **Wyciek sekretu z treści polecenia.** Polecenia i ścieżki mogą zawierać poufne wartości; przechowuj pełny request tylko w prywatnym stanie taska, autoryzuj jego odczyt przez właściciela, redaguj znane wartości Secretów w widokach i nie kopiuj treści requestu do logów ani ogólnych eventów.
- **Zbyt szeroka zgoda dla grupy poleceń.** Pokaż dokładny zakres przed zatwierdzeniem, przechowuj grant w tasku i odrzucaj go po zamknięciu taska.
- **Odmowa pomylona z prośbą o zgodę.** Koreluj ustrukturyzowane źródło i powód z wywołaniem narzędzia; tekst odmowy sam nie wystarcza. Nie oferuj override’u dla jawnej polityki `deny`.
- **Scalanie gubi lokalną zmianę albo nowy token.** Baza w bookkeepingu pozwala odróżnić zmianę lokalną od zmiany źródła; reguły wg typu pliku są wstępne i zostaną zweryfikowane w K8s.
- **Proces agenta modyfikuje konfigurację całego wdrożenia.** Runner i harness nie mają tokenu Kubernetes ani prawa zapisu Secretu; zapis wykonuje tylko sesja edycji w kontrolerze. Wartości Secretu nie trafiają do logów, błędów ani eventów.
- **Limit rozmiaru Secretu (1 MiB).** Cache i klony marketplace pluginów są wykluczone. Do sprawdzenia w K8s: czy Claude Code odtwarza pluginy z rejestru bez cache.
- **Wyścig dwóch decyzji albo odpowiedź po zamknięciu taska.** Wymagaj aktualnego ID oczekiwania i pojedynczego warunkowego przejścia; duplikaty i spóźnione odpowiedzi kończą się czytelnym wynikiem bez grantu.
- **Zgoda Claude Code mylona z dostępem infrastrukturalnym.** UI i kontrakt API rozróżniają zgodę harnessu od ograniczeń K8s, systemu plików i sieci.

### Sources and Existing Evidence

- `docs/sources/permissions.log` — rzeczywiste zdarzenia odmowy, treść polecenia, `tool_use_id` i `permission_decision`.
- `tests/fixtures/claude-code/README.md` i `tests/fixtures/claude-code/` — nagrania Claude Code 2.1.302; README dokumentuje brak `AskUserQuestion` w `--print` i tekstowy fallback. Aktualizuj nagrania o protokół odmowy i wznowienia zamiast polegać na fake harnessie.
- `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs` — bieżąca klasyfikacja `permission_denied` jako elementu technicznego.
- `crates/narsor-engine/src/engine/actions/agent.rs` i `crates/narsor-engine/src/engine/actions/human.rs` — harness działa bez stdin; istnieje trwały zapis oczekiwania, status `InputNeeded` i wznowienie tej samej sesji agenta.
- `crates/narsor-engine/src/domain/workflow_definition.rs`, `crates/narsor-engine/src/workflow/schema.rs` i `crates/narsor-engine/src/workflow/loader.rs` — `workspace_access` jest dziś konkretną wartością z domyślnym `read-write`, więc schema musi zachować informację o pominięciu.
- `crates/narsor-engine/src/engine/actions/harness/claude_code.rs` — adapter obecnie przekazuje tryb uprawnień do Claude Code.
- `src/cli/remote.rs`, `src/tui/task_app/waiting.rs`, `crates/narsor-server/src/api/routes.rs` i `crates/narsor-api-client/src/api/client.rs` — obecne ścieżki wyświetlania i dostarczania decyzji właściciela.
- `crates/narsor-runner/src/runner/{home,storage,init,cli}.rs` — prywatny HOME na storage taska i istniejąca obsługa `CLAUDE_CONFIG_DIR`.
- `crates/narsor-controller/src/kubernetes/manifest.rs` i `deploy/kubernetes/controller-rbac.yaml` — Secret launchu jest tylko do odczytu dla runnera, a kontroler ma obecnie tylko `create` dla Secretów; zapis profilu wymaga nowego, wąsko ograniczonego uprawnienia.
- `docs/solutions/integration-issues/runner-private-home-hides-harness-login-2026-10-10.md` — nie kopiuj historii ani poświadczeń do profilu bazowego; fake harness nie dowodzi zachowania CLI.
- `docs/solutions/integration-issues/claude-code-json-schema-rejects-top-level-anyof-2026-10-17.md` — używaj nagrań prawdziwego CLI do kontraktów protokołu; wynik `subtype` sam nie jest wystarczającym sygnałem błędu.
- `docs/solutions/architecture-patterns/auto-resume-policy-for-lost-workloads-2026-10-11.md` — wiąż decyzje z generacją/ID, obsługuj duplikaty i zachowuj zgodność enumów protokołu.
- [Claude Code: zmienne środowiskowe](https://code.claude.com/docs/en/env-vars) — `CLAUDE_CONFIG_DIR` zmienia domyślny katalog `~/.claude`.
- [Claude Code: headless mode](https://code.claude.com/docs/en/headless) i [CLI reference](https://code.claude.com/docs/en/cli-reference) — `--permission-prompts none` emituje `permission_denied` w `stream-json`; `--allowedTools` przyjmuje reguły narzędzi, a `--resume` wznawia sesję.
- [Claude Code: permissions](https://code.claude.com/docs/en/permissions) — jawne reguły deny są oceniane przed allow rules.
- [Kubernetes: wolumeny](https://kubernetes.io/docs/concepts/storage/volumes/) — wolumen Secretu jest tylko do odczytu.
- [Kubernetes: ServiceAccount](https://kubernetes.io/docs/concepts/security/service-accounts/) — uprawnienia poda do aktualizacji obiektu Secret wynikają z RBAC.
- [Kubernetes: dobre praktyki dla Secretów](https://kubernetes.io/docs/concepts/security/secrets-good-practices/) — przyznawaj dostęp tylko komponentom, które go potrzebują.

Brak repozytoryjnego `STRATEGY.md` i `CONCEPTS.md`; plan używa pojęć z istniejącego modelu taska i dokumentacji operacyjnej.

---

## Implementation Units

### Sequencing

- U1 ustala kontrakt zdarzeń Claude Code i może powstawać niezależnie od U5 i U6.
- U2 zależy od U1, bo zapisuje typowane requesty z tego kontraktu. U3 zależy od U2, bo jego API, CLI, TUI i protokół kontrolera udostępniają i rozstrzygają trwały request. U4 zależy od U1-U3, bo przed wznowieniem Claude Code łączy sklasyfikowaną odmowę z zakresem zgody i trwałą decyzją.
- U5 może powstawać równolegle z U2-U3; przed zakończeniem U4 trzeba zintegrować opcjonalne `workspace_access` na granicy wywołania Claude.
- U6 może powstawać równolegle z U1-U5. U7 zależy od parametrów profilu i kontraktu montowania ustalonych w U6; usunięcie starego przepływu propozycji musi wejść razem z RBAC i montażem Secretu w podzie kontrolera. Najpierw działanie lokalne (U6, część bez K8s), potem K8s.

### U1. Klasyfikacja i rejestracja odmów Claude Code

- **Goal:** rozpoznawać możliwą do rozstrzygnięcia odmowę na podstawie strukturalnych danych Claude Code i zachować jej źródło, polecenie, katalog, sesję oraz ID wywołania.
- **Requirements:** R1-R2, R6, R18; **Flows:** F1, F5; **Acceptance:** AE1, AE3, AE7.
- **Files:** `crates/narsor-engine/src/engine/actions/harness/claude_code/classify.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, `tests/fixtures/claude-code/`, `tests/fixtures/claude-code/README.md`, `tests/claude_code_classification.rs`.
- **Approach:** dodaj typowane dane requestu dla zdarzeń `permission_denied` i końcowego `permission_denials`. Nie klasyfikuj po tekście ani samym `subtype`. Utrzymuj kompatybilność z nieznanymi podtypami oraz dotychczasowymi nagraniami.
- **Test Scenarios:** nagranie z `decision_reason_type` i `tool_use_id` tworzy request z pełną korelacją; jawny deny pozostaje blokadą bez akcji approve; zdarzenie bez wymaganych identyfikatorów pozostaje błędem/elementem technicznym; syntetyczny request `AskUserQuestion` nadal klasyfikuje się jako pytanie agenta, a rzeczywiste nagranie print mode zachowuje tekstowy fallback; brak odpowiedzi nie zmienia się w retry permission; nieznany podtyp nie przerywa parsowania; końcowy wynik nie zamienia błędu Claude w sukces.
- **Verification:** `cargo test --test claude_code_classification --test actions_agent`.
- **Execution note:** nagraj i sprawdź prawdziwe CLI dla wspieranej wersji runtime, z `--permission-prompts none`; potwierdź strukturalną odmowę, brak retry i tekstowy fallback pytania agenta. Oddzielny test fixture sprawdza syntetyczne zdarzenie `AskUserQuestion`. Fake harness nie wystarcza do zatwierdzenia tego kontraktu.

### U2. Trwałe, typowane oczekiwanie permission

- **Goal:** zawieszać wykonanie na prośbie Claude Code i wznawiać je po późniejszej decyzji także po restarcie procesu.
- **Requirements:** R1-R2, R4-R8; **Flows:** F1, F2, F3, F4, F7; **Acceptance:** AE1-AE4.
- **Files:** `crates/narsor-engine/src/engine/actions/human.rs`, `crates/narsor-engine/src/engine/runner.rs`, `crates/narsor-engine/src/persistence/execution_store.rs`, `crates/narsor-engine/src/domain/event.rs`, `tests/actions_human.rs`, `tests/engine_execution.rs`, `tests/engine_recovery.rs`.
- **Approach:** użyj istniejącego pending record i statusu `InputNeeded`, dodając osobną kontynuację permission oraz walidowaną operację rozstrzygnięcia. Operacja zwykłego `HumanInputDelivery` i ogólne resume muszą odrzucać pending permission. Zapis requestu musi poprzedzać zmianę statusu; decyzja musi atomowo sprawdzać bieżące ID i stan taska.
- **Test Scenarios:** odmowa użytkownika wznawia tę samą sesję z tekstową informacją o decyzji; akceptacja zapisuje wybrany scope i wznawia task; po odtworzeniu engine request pozostaje czytelny; stare ID, duplikat oraz odpowiedź po zamknięciu taska nie zmieniają stanu; zwykłe `/input` i `/resume` nie tworzą grantu.
- **Verification:** `cargo test --test actions_human --test engine_execution --test engine_recovery`.

### U3. Decyzje przez API, CLI, TUI i protokół kontrolera

- **Goal:** pozwolić właścicielowi obejrzeć i rozstrzygnąć identyczny pending permission przez dostępne kanały Narsora.
- **Requirements:** R2-R3, R7-R8; **Flows:** F1-F5; **Acceptance:** AE1-AE4.
- **Files:** `crates/narsor-protocol/src/api.rs`, `crates/narsor-protocol/src/controller.rs`, `crates/narsor-server/src/api/routes.rs`, `crates/narsor-engine/src/controller_protocol/`, `crates/narsor-api-client/src/api/client.rs`, `src/cli/remote.rs`, `src/cli/task_text.rs`, `src/tui/task_app/waiting.rs`, `src/tui/task_app/list.rs`, `tests/api.rs`, `tests/auth.rs`, `tests/controller_protocol.rs`, `tests/cli_control_text.rs`, `tests/cli_task_text.rs`, `tests/tui_state.rs`, `tests/tui_render.rs`.
- **Approach:** współdziel DTO i semantykę decyzji; renderuj nieufny tekst polecenia jako dane. Odpowiedź wymaga ID oczekiwania, zakresu jawnie wybranego przez użytkownika i atrybucji użytkownika. Odmów zwracają stan blokady, a nie formularz approve.
- **Test Scenarios:** to samo oczekiwanie i szczegóły są widoczne w API, CLI i TUI; approve z jednego kanału zamyka request obserwowany przez pozostałe; równoczesne approve/deny daje jednego zwycięzcę; spóźnione lub powtórzone zgłoszenie zwraca stan stale/already-answered; niezaufane znaki polecenia nie są interpretowane jako składnia terminala.
- **Verification:** `cargo test --test api --test auth --test controller_protocol --test cli_control_text --test cli_task_text --test tui_state --test tui_render`.

### U4. Grant taskowy i wznowienie Claude Code

- **Goal:** ponowić pracę z grantem ograniczonym do zatwierdzonego polecenia lub wspieranego typu poleceń w bieżącym tasku.
- **Requirements:** R3-R7; **Flows:** F2-F5; **Acceptance:** AE1-AE3.
- **Files:** `crates/narsor-engine/src/engine/actions/agent.rs`, `crates/narsor-engine/src/engine/actions/harness/mod.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code/tools.rs`, `tests/actions_agent.rs`, `tests/agent_harness_selection.rs`, `tests/fixtures/claude-code/`.
- **Approach:** zapisuj grant w trwałym stanie taska i przekazuj jako oddzielne argumenty CLI podczas wznowienia zachowanej sesji. Nie buduj argumentów przez shell string. Pozostałe adaptery nie mogą otrzymać grantów Claude przez współdzielony, niejawny parser.
- **Test Scenarios:** dokładny grant pozwala wznowionej sesji na zatwierdzone polecenie; reguła grupowa dopasowuje tylko zatwierdzony typ; dopisane argumenty, zmieniony katalog, prefiksy podobne do zatwierdzonych oraz `;`, `&&`, pipe, redirection, substitution, glob i quoting nie mogą rozszerzyć grantu; task drugi nie dziedziczy żadnego grantu; zamknięcie lub usunięcie taska kasuje możliwość ponownego użycia; jawne deny nadal blokuje polecenie nawet przy allow argumentach; brak lub odrzucenie wznowienia kończy się widoczną blokadą, nie szerszym trybem.
- **Verification:** `cargo test --test actions_agent --test agent_harness_selection` oraz nagranie prawdziwego CLI dla odmowy, wznowienia i ponownego wywołania.

### U5. Jawne i pominięte `workspace_access`

- **Goal:** zachować różnicę między wartością podaną w workflow a wartością pominiętą, bez zmiany interpretacji zapisanych wykonań.
- **Requirements:** R9-R10, R18; **Acceptance:** AE4, AE7.
- **Files:** `crates/narsor-engine/src/workflow/schema.rs`, `crates/narsor-engine/src/workflow/loader.rs`, `crates/narsor-engine/src/domain/workflow_definition.rs`, `crates/narsor-engine/src/engine/actions/harness/mod.rs`, `crates/narsor-engine/src/engine/actions/harness/claude_code.rs`, adaptery pozostałych harnessów, `tests/schema_loading.rs`, `tests/workflows.rs`, `tests/agent_harness_selection.rs`.
- **Approach:** zachowaj presence bit/optional na granicy parsowania workflow i przekaż brak trybu jako brak override’u. Jawny `read-only` zawsze ogranicza zapis, a jawny `read-write` wybiera tryb zapisu adaptera. Stare rekordy z zapisanym `read-write` pozostają zgodne.
- **Test Scenarios:** nowy workflow bez pola nie dodaje flagi/ustawienia adaptera; jawne `read-only` i `read-write` mapują się zgodnie z kontraktem; rekord wykonania sprzed zmiany zachowuje dawną wartość; Claude respektuje ustawienia harnessu przy pominięciu; pozostałe harnessy nie dostają przypadkowej zmiany zachowania.
- **Verification:** `cargo test --test schema_loading --test workflows --test agent_harness_selection`.

### U6. Profil harnessu w runnerach

- **Goal:** przygotować katalog roboczy profilu przed każdym startem harnessu: bez kopiowania lokalnie, ze scalaniem w K8s.
- **Requirements:** R11-R14, R17; **Flows:** F6-F7; **Acceptance:** AE5.
- **Files:** konfiguracja Narsora (parametry katalogu źródłowego i roboczego), `crates/narsor-controller/src/kubernetes/manifest.rs`, `crates/narsor-runner/src/runner/storage.rs`, `crates/narsor-runner/src/runner/mod.rs`, `crates/narsor-runner/src/runner/cli.rs`, `deploy/kubernetes/admission-policy.yaml`, `docs/operations/remote-backends.md`, `tests/kubernetes_adapter.rs`, `tests/runner.rs`.
- **Approach:** dodaj dwa parametry profilu i regułę „brak kopiowania, gdy źródło puste albo równe roboczemu”. Zastąp jednorazowe przypięcie kopii (marker z `d27ca92`) scalaniem trójstronnym z KTD6 przy każdym starcie. Pomijaj wewnętrzne wpisy wolumenu Secretu. Pliki poświadczeń zapisuj z prawami tylko dla właściciela.
- **Test Scenarios:** puste źródło i źródło równe roboczemu nie kopiują; świeży task kopiuje wszystkie pliki profilu bez `..data`; zmiana tylko lokalna jest zachowana po restarcie; zmiana tylko w źródle (nowy token) trafia do katalogu roboczego; zmiana po obu stronach `settings.json` scala klucze i sumuje reguły; zmiana po obu stronach `.credentials.json` bierze wersję ze źródła; sesje i historia w katalogu roboczym pozostają bez zmian.
- **Verification:** `cargo test --test kubernetes_adapter --test runner` oraz testy jednostkowe scalania.
- **Status:** `d27ca92` montuje Secret read-only w każdym podzie z profilem i kopiuje pliki jednorazowo; kopiuje też wewnętrzne katalogi wolumenu Secretu (`..data`, `..<timestamp>`) — do poprawy w tej jednostce.

### U7. Edycja profilu w kontrolerze

- **Goal:** pozwolić użytkownikowi zmienić profil w harnessie uruchomionym przez kontroler i zapisać wynik do Secretu.
- **Requirements:** R15-R17; **Flow:** F8; **Acceptance:** AE6.
- **Files:** `crates/narsor-controller/src/` (komenda edycji, rdzeń edycji, zapis Secretu), `crates/narsor-controller/src/kubernetes/`, `crates/narsor-controller-runtime/src/`, `crates/narsor-protocol/src/controller.rs`, `crates/narsor-engine/src/controller_protocol/`, `crates/narsor-api-client/src/api/client.rs`, `deploy/kubernetes/controller-rbac.yaml`, `deploy/kubernetes/` (montaż Secretu w podzie kontrolera), `docs/operations/remote-backends.md`, `tests/kubernetes_adapter.rs`, `tests/controller_protocol.rs`.
- **Approach:** zbuduj rdzeń edycji niezależny od transportu terminala: kopia profilu do katalogu tymczasowego, uruchomienie harnessu z `CLAUDE_CONFIG_DIR`, zbieranie plików wg reguł include/exclude, zapis do skonfigurowanego Secretu (ostatni zapis wygrywa). Pierwszy transport: komenda kontrolera dla `kubectl exec -it`. Usuń stary przepływ propozycji i akceptacji (endpointy, typy protokołu, kolejkę i walidację tylko `permissions`). Lokalnie edycja nie jest potrzebna: harness zmienia standardowy katalog bezpośrednio.
- **Test Scenarios:** rdzeń edycji z fałszywym harnessem zmienia `settings.json`, dodaje skill i zapisuje je; `projects/` i historia nie są zbierane; zapis trafia wyłącznie do skonfigurowanego Secretu; RBAC daje `get`/`update` tylko kontrolerowi i tylko dla nazwanego Secretu; wartości Secretu nie trafiają do logów ani błędów.
- **Verification:** `cargo test --test kubernetes_adapter --test controller_protocol` oraz przegląd manifestów RBAC i admission policy.
- **Execution note:** szczegóły K8s (transport, format Secretu, reguły scalania) są wstępne; dopracować je w praktycznych testach w K8s, priorytetem jest działanie lokalne.

---

## Verification Contract

| Scope | Verification | Done signal |
| --- | --- | --- |
| Claude permission protocol | `cargo test --test claude_code_classification --test actions_agent` plus recorded runs of the supported real Claude CLI | Structured denial, deny-vs-approvable distinction, session/tool correlation and resumed grant all match the supported CLI. |
| Durable request and channel parity | `cargo test --test actions_human --test engine_execution --test engine_recovery --test api --test auth --test controller_protocol --test cli_control_text --test cli_task_text --test tui_state --test tui_render` | One durable typed request is visible and answerable through all existing owner surfaces; stale, duplicate and unauthorized decisions cannot grant access. |
| Workflow compatibility | `cargo test --test schema_loading --test workflows --test agent_harness_selection` | Explicit and omitted values differ for newly loaded workflows, while old persisted records and other harnesses retain defined behavior. |
| Harness profile lifecycle | `cargo test --test kubernetes_adapter --test runner` plus merge unit tests | No copy when source is empty or equal to the work dir; Secret mount, three-way merge on restart, controller-only Secret writes and access boundaries pass. |
| Full repository | `cargo fmt --check` and `cargo test --workspace` | Formatting and workspace tests pass with the feature enabled. |

## Definition of Done

- A task can wait on a resolvable Claude Code permission request and accept or refuse it later without an interactive terminal.
- Only the current task receives the approved scope; explicit policy denies, sandbox limits and infrastructure permissions remain enforced.
- The pending request and its attribution survive runner and control-plane recovery, and all existing owner channels observe the same decision state.
- Omitted `workspace_access` remains distinct from explicit values, with legacy persisted executions preserving their previous interpretation.
- A local installation runs the harness on its standard directory without copying; every Claude Code K8s runner receives the dedicated profile Secret read-only and merges it into a private writable work directory on each start, without distributing local changes.
- Only the Narsor controller edits the Secret: it copies the profile, runs the harness for the user, collects the profile files and writes them back; external Secret changes are accepted and the last write wins.
- The shared profile supports authentication, permissions, MCP, plugins, and skills without including task session history.
- Other harness permission behavior remains unchanged, documentation describes installation and operations, and the verification contract passes.

---
