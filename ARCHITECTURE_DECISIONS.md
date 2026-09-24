# Architecture Decisions - AI Workflow Narsor

Dokument zawiera zestawienie kluczowych decyzji architektonicznych podjętych dla projektu AI Workflow Narsor w fazie MVP.

## Tabela decyzji

| ID  | Obszar | Decyzja | Alternatywa | Status | Konsekwencje |
|-----|--------|---------|-------------|--------|--------------|
| D1  | Język implementacji | **Rust** (z frameworkiem Axum) | Go, TypeScript, Python | ✅ ZATWIERDZONA | Bezpieczeństwo pamięci, wydajność, pojedynczy plik binarny, łatwe TUI i API. |
| D2  | Architektura procesu | **Single Process** (jeden proces narsora) | Mikroserwisy | ✅ ZATWIERDZONA | Prostszy deployment, brak opóźnień sieciowych, wszystkie komponenty w jednej binarynce. |
| D3  | Persistence | **Filesystem-based storage** (pliki JSON + foldery) | SQLite, PostgreSQL | ✅ ZATWIERDZONA | Zero zależności DB, łatwy backup (kopia folderu), struktura: `/tasks/{id}/{state,context,events/,artifacts/}`. |
| D4  | Event History | **Hybryda**: Bieżący stan (`state.json`) + Append-only log (`events/`) | Pełny Event Sourcing | ✅ ZATWIERDZONA | Szybki odczyt stanu, pełna historia do debugowania, możliwość migracji do ES w przyszłości. |
| D5  | Workflow DSL | **YAML** + walidacja JSON Schema | JSON, TOML, własny DSL | ✅ ZATWIERDZONA | Czytelność dla ludzi, łatwa walidacja, brak potrzeby parsera własnego języka. |
| D6  | Wyrażenia i warunki | **Static Output Mapping** (stałe etykiety wyjść) + JSON Contract | Dynamiczne wyrażenia (CEL, JMESPath) | ✅ ZATWIERDZONA | Action zwraca `{ output_label, variables, artifacts }`. Mapowanie `LABEL: next_state` w YAML. Brak silnika wyrażeń. |
| D7  | State vs Action | **1 State = 1 Action** | Pipeline akcji w stanie | ✅ ZATWIERDZONA | Prostota modelu. Jeśli potrzeba sekwencji, definiuje się jako osobne stany. |
| D8  | Parametry wejściowe | **Brak pre-hooków**. Akcja sama pobiera potrzebne dane z kontekstu/repo. | Dynamiczne kroki przygotowawcze | ✅ ZATWIERDZONA | Uproszczenie engine'a. Skrypt/Agent sam odpowiada za fetchowanie danych (np. nazwa brancha). |
| D9  | Dynamiczne stany | **Graf statyczny**. Brak modyfikacji definicji w trakcie execution. | Dynamiczne dodawanie stanów przez AI | ✅ ZATWIERDZONA | Przewidywalność, łatwość walidacji przed startem. AI wybiera tylko spośród istniejących przejść. |
| D10 | Modyfikacja Context | **Engine zatwierdza zmiany**. Agent proponuje patch (`variables` w wyniku). | Bezpośredni zapis agenta | ✅ ZATWIERDZONA | Bezpieczeństwo spójności danych. Engine weryfikuje i aplikuje zmiany do `context.json`. |
| D11 | Schema Context | **JSON** (dowolna struktura) z opcjonalną walidacją | Silnie typowany schema | ⚠️ CAN DEFER | Elastyczność w MVP. Walidacja może być dodana później na poziomie definicji workflow. |
| D12 | Storage Artefaktów | **Filesystem** (`artifacts/` w folderze taska) + metadata JSON | S3, Baza danych | ✅ ZATWIERDZONA | Prosty dostęp, łatwość debugowania. Abstrakcja `ArtifactStore` pozwoli przyszłą zmianę na S3. |
| D13 | Lifecycle Workspace | **Tworzony per Execution**, usuwany po zakończeniu (z opcją `preserve`). | Współdzielone workspace'y | ✅ ZATWIERDZONA | Izolacja równoległych wykonań. Czystka po zakończeniu (z weryfikacją pusha). |
| D14 | Izolacja Agentów | **Docker Container** + izolacja filesystemu | Sandbox, VM, bubblewrap | ✅ ZATWIERDZONA | Uruchomienie całości w kontenerze. Izolacja poprzez strukturę folderów i uprawnienia FS. |
| D15 | Uprawnienia | **Izolacja fizyczna (FS)** + montowanie RO/RW | Silnik reguł capabilities | ✅ ZATWIERDZONA | Wszystkie repo obok siebie w `/code/`. Główny repo RW, inne RO. Ochrona przed przypadkową modyfikacją. |
| D16 | Sekrety | **Environment Variables** / plik `.env` (montowany) | Vault, Secret Manager | ✅ ZATWIERDZONA | Proste zarządzanie w Docker. Sekrety nie trafiają do logów ani contextu. |
| D17 | Agent Runtime API | **Analiza przed implementacją** (sprawdzenie CLI Claude Code/OpenCode) | Projektowanie "na papierze" | ✅ ZATWIERDZONA | Interfejs dopasowany do rzeczywistych możliwości harnessów (tryb batch, stream, resume). |
| D18 | Wznawianie Sesji | **Priorytet: `--resume`**. Fallback: Nowa sesja z logiem. | Tylko nowe sesje | ✅ ZATWIERDZONA | Próba podtrzymania kontekstu rozmowy. Jeśli niemożliwe – kontynuacja na podstawie historii w plikach. |
| D19 | Output Agenta | **Structured JSON** (`{ output_label, data, variables }`) | Parsowanie tekstu (keyword fallback) | ✅ ZATWIERDZONA | Wymuszenie struktury na agencie. Fallback tylko jako ostateczność. |
| D20 | Błędny Output | **Retry N razy** → AgentDecision/HumanDecision → Failure | Natychmiastowy failure | ✅ ZATWIERDZONA | Tolerancja na halucynacje LLM. Eskalacja do człowieka jeśli agent nie poprawi formatu. |
| D21 | Semantyka Retry | **Hierarchiczna**: Action Retry, Agent Retry, Workflow Retry. | Globalny retry count | ✅ ZATWIERDZONA | Precyzyjna kontrola nad tym, co jest powtarzane (np. tylko sieć vs cały krok). |
| D22 | Timeouty | Definiowane per Action/Wait. Po timeout: retry/failure/alternative transition. | Globalny timeout | ⚠️ IMPLEMENTATION DETAIL | Szczegóły konfiguracji w YAML. |
| D23 | Recovery po crashu | **Status UNKNOWN** + reconcilation na podstawie logów. | Automatyczny rollback | ✅ ZATWIERDZONA | Bezpieczne wznowienie. Ręczna lub automatyczna weryfikacja stanu zewnętrznych systemów. |
| D24 | Exactly-once | **At-least-once** + idempotency keys (execution_id/action_id). | Exactly-once | ✅ ZATWIERDZONA | Realistyczne podejście dla operacji zewnętrznych (Git, HTTP). Unikanie duplikatów przez ID. |
| D25 | Delivery Eventów | **At-least-once** + deduplikacja po event_id. | Exactly-once | ✅ ZATWIERDZONA | Gwarancja dostarczenia przy restarcie. Odbiorca musi obsługiwać duplikaty. |
| D26 | External Events | **Polling Service** (odpytywanie zewnętrznych API) | Webhooki / Nasłuchiwanie | ✅ ZATWIERDZONA | Workflow rejestruje oczekiwanie (np. na PR). Usługa pyta GitHub i wstrzykuje event do workflow. Brak serwera webhooków w MVP. |
| D27 | GitHub Integration | **Minimalna**: PR create/merge, status CI. | Pełna integracja | ⚠️ CAN DEFER | Tylko niezbędne operacje dla MVP. Reszta jako rozszerzenie. |
| D28 | Narsor Assistant | **Brak w MVP**. Tylko Human-in-the-loop. | Agent-asystent | ✅ ZATWIERDZONA | Fokus na silniku workflow. Asystent jako klient API w przyszłości. |
| D29 | Permissions Asystenta | N/A (brak asystenta w MVP). | - | ⚠️ CAN DEFER | Do określenia przy dodawaniu asystenta. |
| D30 | Manual Transition | **Tryb mieszany**: 1) Wybór z listy (standard), 2) Override (dowolny stan, wymagany flag). | Tylko z listy / Dowolny | ✅ ZATWIERDZONA | Bezpieczeństwo domyślne, elastyczność w sytuacjach awaryjnych (override). |
| D31 | Mutacja Workflow | **Zabroniona** podczas execution. | Dynamiczna zmiana definicji | ✅ ZATWIERDZONA | Stabilność wykonania. Zmiana wymaga nowego execution. |
| D32 | Równoległość w Workflow | **Brak w MVP** (sekwencyjne). | Parallel branches | ✅ ZATWIERDZONA | Uproszczenie engine'a. Równoległość realizowana jako osobne execution lub przez pod-agentów w prompcie. |
| D33 | Cancellation | **Best effort** + sygnał do agenta/skryptu. | Gwarantowane przerwanie | ⚠️ IMPLEMENTATION DETAIL | Próba grzecznego zatrzymania. Wymagana obsługa SIGINT w skryptach. |
| D34 | Pause Semantics | **Pause = nie zaczynaj kolejnej akcji**. Interrupt = przerwij aktualną. | Tylko pause globalny | ✅ ZATWIERDZONA | Rozróżnienie między "zatrzymaj proces" a "poczekaj przed następnym krokiem". |
| D35 | Multi-user | **Single-user / Local** w MVP. | Multi-user z rolami | ✅ ZATWIERDZONA | Brak auth/authz w MVP. Architektura plików nie blokuje późniejszego dodania użytkowników. |
| D36 | Web UI | **TUI (pełnookienkowe) + API** w MVP. Web później. | Web od razu | ✅ ZATWIERDZONA | Fokus na backend i TUI. API wspólne dla wszystkich klientów. |
| D37 | Transport API | **REST** (operacje) + **SSE** (eventy real-time). | GraphQL, gRPC, WebSocket | ✅ ZATWIERDZONA | Prostota REST, lekkość SSE dla push events. Łatwa integracja z TUI. |
| D38 | Real-time Updates | **SSE** (Server-Sent Events). | WebSocket, Polling | ✅ ZATWIERDZONA | Wystarczające dla logów i statusu. Mniejszy narzut niż WebSocket. |
| D39 | Local Folder | **Obsługiwane** (workflow bez repo Git). | Tylko Git repos | ✅ ZATWIERDZONA | Możliwość pracy w dowolnym folderze (`Workspace = local directory`). |
| D40 | Zmiana Repozytorium | **Brak jawnej zmiany w workflow**. Stały workspace per execution. | Jawne przełączanie w YAML | ✅ ZATWIERDZONA | Jeśli agent potrzebuje innego repo, robi to sam (clone) lub jako podzadanie. |
| D41 | Transfer Kontekstu | **Przez Artefakty/Context**. Brak automatycznego kopiowania plików między repo. | Automatyczny sync | ✅ ZATWIERDZONA | Jawne przekazywanie danych (np. plik configu) jako artifact. |
| D42 | Komunikacja Agentów | **Pośrednia** (przez Context/Artifacts). | Bezpośrednia komunikacja | ✅ ZATWIERDZONA | Agent A zapisuje wynik → Engine → Agent A czyta kontekst. |
| D43 | Agent uruchamia Agenta | **Nie bezpośrednio**. Agent sugeruje, Engine decyduje i uruchamia. | Delegowanie przez agenta | ✅ ZATWIERDZONA | Kontrola nad przepływem w rękach Engine'a. |
| D44 | Dynamiczne Workflow (AI) | **Brak w MVP**. Workflow definiowane z góry. | Generowanie przez AI | ✅ ZATWIERDZONA | Stabilność i przewidywalność. AI może pomóc stworzyć YAML, ale nie zmienia go w locie. |
| D45 | Policy Engine | **Wbudowany w Engine** (proste reguły). | Osobny komponent | ⚠️ CAN DEFER | Interfejs wydzielony, ale implementacja w core. Możliwość wydzielenia później. |
| D46 | Observability | **Structured Logs + Event History**. | Distributed Tracing, Metrics | ✅ ZATWIERDZONA | Wystarczające do debugowania i audytu w MVP. |

## Podsumowanie strategii MVP

1. **Core**: Deterministyczny silnik state machine w Rust (single process).
2. **Persistence**: Płaska struktura plików JSON i folderów (bez bazy danych).
3. **Workflow**: YAML ze statycznym grafem i mapowaniem wyjść (bez wyrażeń).
4. **Agent**: Uruchamiany w Dockerze, izolowany filesystemowo (RO/RW), komunikacja przez JSON.
5. **Events**: Polling service dla zdarzeń zewnętrznych (brak webhooków), SSE dla UI.
6. **UX**: TUI jako główny interfejs, Web w planach.
7. **Bezpieczeństwo**: Izolacja przez kontener i uprawnienia plików, sekrety przez ENV.

## Następne kroki

1. ~~Zatwierdzenie niniejszego dokumentu.~~ — zrobione.
2. ~~Implementacja fazy 1 (Domain Model).~~ — zrobione; `src/domain/` zawiera
   komplet typów z `DOMAIN_MODEL.md`.
3. ~~Faza 2 (MVP silnika).~~ — zrobione zgodnie z
   `docs/plans/2026-09-23-001-feat-workflow-engine-mvp-plan.md`: parser i
   walidator YAML, persystencja plikowa, pętla silnika ze `ScriptAction` i
   `HumanAction`, substytucja szablonów, cykl życia workspace, CLI i TUI.
4. Analiza CLI Claude Code/OpenCode (D17) przed implementacją adapterów —
   **to jest najbliższy kolejny krok.** Blokuje `AgentAction` (D17–D20) oraz
   `AgentDecision`.
5. Dalsza część fazy 3: `WaitAction` i polling zdarzeń (D26) wraz z integracją
   GitHub (D27), REST API i SSE (D37, D38), konteneryzacja (D14, D16).

> Uwaga: dokument odwoływał się wcześniej do `IMPLEMENTATION_PLAN.md`, którego
> w repozytorium nigdy nie było. Planem obowiązującym dla fazy 2 jest ten
> wskazany w punkcie 3.
