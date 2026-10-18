# Zdalne backendy — pozostałe prace

Plan: `docs/plans/2026-10-01-2329-feat-zdalne-backendy-wykonawcze-plan.md`
Gałąź: `feat/remote-backends` (`6fde9a3..ec0e617`, U1–U10)

Dokument zbiera to, co po zakończeniu U1–U10 pozostało do zrobienia.
Obszary wymagające code review opisuje osobny dokument
`docs/residual-review-findings/feat-remote-backends-review-needed.md`.
Tutaj są wyłącznie prace do wykonania: brakujące funkcje, weryfikacje i kroki wdrożeniowe.

## 1. Przed scaleniem / wydaniem

- [ ] **Code review i uproszczenia.** Wykonać przegląd według dokumentu „review needed”, potem `/simplify`. Naprawić znalezione błędy.
- [ ] **Test cyklu na żywym klastrze (Verification Contract).** Przejść pełny cykl create → pause → resume → archive na jednorazowym namespace z prawdziwą storage class, service accountem controllera i testowym issuerem OIDC. Obecny `live_cluster_smoke` (`NARSOR_K8S_SMOKE_*`) jest tylko do odczytu i sprawdza osiągalność API oraz storage class.
- [ ] **Walidacja polityki admission.** Zastosować `deploy/kubernetes/admission-policy.yaml` na Kubernetes ≥ 1.30 i sprawdzić, że wrogie specyfikacje Pod/PVC są odrzucane. CEL był dotąd weryfikowany tylko strukturalnie i przez lustrzane `check_pod_shape` w Rust.
- [ ] **Digest obrazu controllera.** `deploy/kubernetes/controller.yaml` ma zerowy digest. Trzeba go ustawić przy każdym wydaniu i dodać to do procesu release.
- [ ] **Obrazy runtime bundle i narzędzi.** Zbudować i opublikować obraz OCI runtime bundle'a: payload w `/narsor`, w tym `bin/narsor` (statyczny lub zgodny z libc obrazów bazowych), oraz `cp` zdolny wykonać `cp -a`. Dotyczy też obrazów narzędzi. Plan (R20) zakłada ich istnienie, ale pipeline budowania nie powstał.
- [ ] **Zależności runtime bundle'a dla harnessów.** Pytanie odroczone w planie: jakie dokładne zależności muszą być w bundle'u, żeby Claude Code, Codex i OpenCode działały na obrazach Ubuntu/Debian, .NET SDK i Alpine/minimal. Wymaga pomiaru na reprezentatywnych obrazach.
- [ ] **Opcjonalny obraz bazowy Ubuntu (R40).** Opublikować wygodny, zgodny z kontraktem obraz bazowy Narsora. Nie jest obowiązkowy.
- [ ] **Kontrole przed wystawieniem na sieć (Definition of Done).** Do sprawdzenia:
  - TLS na reverse proxy (`X-Forwarded-Proto: https` na każdym hopie, bez przepisywania ścieżki/query/body, długie SSE),
  - konfiguracja OIDC,
  - procedura unieważnienia controllera,
  - RBAC i admission,
  - quoty namespace.

## 2. Brakujące funkcje klienta

- [ ] **Logowanie OIDC.** Brak flow device-code. Obecnie token dostarcza zewnętrzny helper przez `--token-file`, `--token-command` lub `NARSOR_TOKEN`. Do dodania `narsor auth login` z device-code i odświeżaniem tokenu.
- [ ] **Tworzenie tasków w zdalnym TUI.** Dziś tylko CLI (`narsor task create`).
- [ ] **Endpoint listy backendów.** `GET /control/v1/backends` z id, rodzajem, limitami i stanem controllera, bez danych wrażliwych. Picker domyślnego backendu w TUI pokazuje dziś tylko znane wartości plus wpisywanie ręczne.
- [ ] **Materiały w TUI.** Upload, lista i pobieranie są dziś tylko w CLI (`narsor task materials …`).
- [ ] **Natywny TLS (opcjonalnie).** Tryb sieciowy wymaga dziś zaufanego proxy. Natywny rustls z certyfikatem i kluczem w konfiguracji to możliwe rozszerzenie.

## 3. Luki funkcjonalne po stronie serwera i runnera

- [ ] **Per-task właściciel dla tras `/executions`.** Starsze lokalne trasy mają tylko gate pojedynczego właściciela, bez autoryzacji per task.
- [ ] **Obrazy per krok na Kubernetesie.** `step_images` są dziś odrzucane przez adapter v1. Trzeba zdecydować, czy wspierać inny obraz dla kroku (np. osobny Pod per krok na tym samym PVC), czy pozostać przy odrzuceniu.
- [x] **Conductor w runnerze.** Zamknięte w U5 planu przeprojektowania TUI (`docs/plans/2026-10-09-001-feat-tui-przeprojektowanie-plan.md`). Runner składa conductora (`src/conductor/assembly.rs`, wspólnie z daemonem), gdy harness zadania da się uruchomić w jego środowisku (binarka na `PATH`; obraz Kubernetesa dostarcza ją sam, adapter niczego nie konfiguruje). Bez harnessu snapshot niesie `conductor_available = false` z powodem, a nieudany krok trafia do właściciela bez diagnozy i propozycji. Czat, propozycje i cofanie decyzji idą przez `POST /control/v1/tasks/:id/operator-commands`, stan czyta się z `GET /control/v1/tasks/:id/conductor`, CLI to `narsor task conductor …`. (Wybór harnessu dyrygenta per zadanie usunięto później: dyrygent podąża za domyślnym harnessem instalacji — D56.)
- [ ] **Poświadczenia Git przez SSH.** Klon w runnerze obsługuje tylko tokeny HTTPS (helper z użytkownikiem `x-access-token`). Brak obsługi kluczy SSH.
- [ ] **Długie offline runnera.** Po przekroczeniu TTL poświadczenia (15 min) runner dostaje 401, dopóki controller nie zamontuje nowego. Controller powinien automatycznie odświeżać Secret z poświadczeniem dla działającej inkarnacji.
- [ ] **Unieważnienie long-polla controllera.** Long-poll już w toku kończy się dopiero po swoim limicie (30 s), nie natychmiast po unieważnieniu.
- [ ] **Fencing zapisów do backendu.** Controller sprawdza lease na początku kroku. Warto dodać warunki wstępne na zapisach do Kubernetesa, np. etykietę z epoką leasa w warunkach wstępnych.
- [ ] **`flock` na sieciowych systemach plików.** Blokady stanu centrali i task storage'u opierają się na `flock`. Trzeba udokumentować wymagania wobec wolumenu albo dodać alternatywę.
- [x] **Zablokowany slot przez porzucony task.** Rozwiązane w zakresie zakończonych i czekających tasków przez D62 (`ARCHITECTURE_DECISIONS.md`; źródło: `docs/brainstorms/2026-10-18-zwalnianie-slotow-wg-stanu-wykonania-requirements.md`, plan: `docs/plans/2026-10-18-002-feat-zwalnianie-slotow-i-runner-do-odczytu-plan.md`). Centrala zwalnia slot tasku z `Completed`/`Cancelled` od razu, a z `InputNeeded`/`Failed` po terminie z ustawień backendu (`slot_release_grace_secs`), więc zakończony lub porzucony task nie blokuje kolejki; D62 zmienia dawne celowe trzymanie slotu. Od D58 zadanie `Interrupted` jest wznawiane automatycznie. Zostaje trzymanie slotu przez zadanie z wyczerpanymi próbami wznowienia, utracone podczas pauzy albo `Unknown` (dopóki controller nie potwierdzi utraty) oraz `Interrupted` z ostatnim snapshotem `Completed` (zwalnia slot po wznowieniu); dla nich nadal można rozważyć przypomnienie lub widoczny alert dla właściciela. Odczyt szczegółów zatrzymanego taska czyta runner tylko do odczytu na każdym backendzie (D63, faza 2; zastępuje D59). Dawny stan przejściowy „odczyt przez D59” nie obowiązuje.
- [ ] **Blokujące oczekiwanie w `LocalBackend`.** `wait_gone` czeka do 2 s synchronicznie w kontekście async.
- [ ] **Polityka retencji PV.** Adapter nie widzi `reclaimPolicy` pojedynczego PV. Admin, który ustawi `Retain` na PV, obchodzi gwarancję, że archiwizacja niszczy dane. Wymaga uprawnień do PV albo procedury operatora.

## 4. Odroczone świadomie przez plan (osobne plany / brainstormy)

- [ ] **Rewizje workspace'u (R24–R28, F2, AE6).** Pobranie po obowiązkowej pauzie, manifest i delta, lokalna materializacja, odesłanie korekty i rozwiązywanie konfliktów. Wymaga osobnego planu bezpieczeństwa i synchronizacji: walidacja ścieżek i dowiązań, ochrona `.git`, wykluczanie sekretów, podgląd manifestu.
- [ ] **Przekazanie taska między użytkownikami.** Zespoły, rola lidera, nowa sesja po przejęciu, zawartość pakietu przekazania. Model własności jest już zmienny (`OwnerStore::rebind`), ale nie ma API transferu.
- [ ] **Semantyka historii po archiwizacji.** Do późniejszego brainstormingu.
- [ ] **Lokalny Docker i adaptery chmurowe.** Kontrakt (`ExecutionBackend` + `contract_suite!`) jest gotowy na kolejne adaptery.
- [ ] **Obrazy pochodne.** Ewentualna materializacja `ResolvedEnvironment` jako obrazu pochodnego, jeśli pomiary czasu startu to uzasadnią.
- [ ] **HA centrali, zewnętrzna baza i kolejka.** Poza pierwszym zakresem.

## 5. Dokumentacja

- [ ] Uzupełnić `docs/operations/remote-backends.md` po teście na żywym klastrze o rzeczywiste wymagania storage class, quoty i obserwowane czasy startu.
- [ ] Opisać proces budowania i wydawania runtime bundle'a, obrazów narzędzi oraz controllera.
