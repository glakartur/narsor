# Odroczone uprawnienia harnessu — pozostałe prace

Plan: `docs/plans/2026-10-17-1520-feat-deferred-harness-permissions-plan.md`
Gałąź: `feat/deferred-harness-permissions` (U6: `847711a`, U7: `2e7bc29..5a09285`)

Dokument zbiera prace, które zostały po U6 i U7. Pierwsza pozycja to znany
problem bezpieczeństwa, który świadomie odroczono.

## 1. Izolacja sesji edycji profilu od poświadczeń kontrolera (R15)

- [ ] **Uruchomić Claude Code sesji edycji poza zasięgiem poświadczeń kontrolera.**

**Stan obecny.** `narsor-controller edit-claude-profile` uruchamia Claude Code
w kontenerze kontrolera, pod tym samym UID (65532). W tym kontenerze są:

- token ServiceAccount kontrolera
  (`/var/run/secrets/kubernetes.io/serviceaccount/token`,
  `automountServiceAccountToken: true`). Token daje prawo tworzyć i usuwać
  Pody i PVC oraz zapisywać Secret profilu;
- poświadczenie kontrolera `nsk.…` w `/var/lib/narsor-controller`.

Procesy sesji mogą odczytać oba pliki. Dotyczy to agenta (narzędzie Bash),
hooków, serwerów MCP, pluginów, `statusLine` i `apiKeyHelper`. To narusza R15
(„harness nie dostaje uprawnień Kubernetes”). Commit `527c389` usunął tylko
zmienne środowiskowe kontrolera z procesu harnessu. Dostępu do plików to nie
zamyka.

**Dlaczego reguły uprawnień Claude Code nie wystarczą.**

- Reguły `deny` dotyczą tylko narzędzi agenta. Hooki, serwery MCP i pluginy
  to osobne procesy bez kontroli uprawnień, a sesja edycji służy właśnie do
  ich zmiany.
- Reguła w `settings.json` sesji jest edytowalna w tej samej sesji. Twardą
  regułę daje tylko plik managed settings w obrazie, ale ograniczenie z
  poprzedniego punktu nadal obowiązuje.
- Sandbox Claude Code (bubblewrap) w Podzie wymaga zwykle przestrzeni nazw
  użytkownika, których `RuntimeDefault` i polityki klastra często nie
  dopuszczają. Sandbox nie obejmuje procesów MCP i hooków.

**Możliwe rozwiązania.**

- **a) Osobny Pod sesji (zalecane).** Kontroler tworzy efemeryczny Pod bez
  tokenu ServiceAccount i bez wolumenu stanu kontrolera, z Claude Code w
  środku. Użytkownik wchodzi do niego przez `kubectl exec -it`. Po zamknięciu
  harnessu kontroler pobiera pliki profilu, pokazuje przegląd zmian i sam
  zapisuje Secret. Rdzeń `crates/narsor-controller/src/profile_edit.rs` jest
  niezależny od transportu, więc zmiana dotyczy głównie transportu w
  `controller.rs`, RBAC (tworzenie i usuwanie Poda sesji, polityka admission
  dla jego kształtu) i dokumentacji.
- **b) Ten sam Pod, inny UID.** Harness działa w drugim kontenerze albo pod
  innym UID, który nie ma dostępu do tokenu i stanu kontrolera. Mniej kodu,
  ale izolacja zależy od uprawnień plików i obrazu.

Managed settings z regułami `deny` oraz sandbox mogą być dodatkową warstwą
dla a) albo b), ale nie zastępują izolacji.

**Do czasu poprawki.** Używać `edit-claude-profile` tylko z zaufanymi
pluginami, serwerami MCP i skills. Przegląd zmian przed zapisem (diff,
ostrzeżenia, brak `--yes` dla zmian innych niż logowanie i bezpieczne
ustawienia) chroni Secret przed niezauważoną zmianą, ale nie chroni tokenu.

## 2. Weryfikacja w Kubernetesie

- [ ] **Sesja edycji na żywym klastrze.** Sprawdzić `kubectl exec -it` z
  prawdziwym Claude Code: logowanie OAuth, zapis Secretu, limit pamięci 1Gi,
  `emptyDir` na `/tmp`. Obraz kontrolera musi zawierać `claude`.
- [ ] **Format Secretu.** Plan zostawia wybór między mapowaniami `key=path`
  (stan obecny) a jednym kluczem z archiwum profilu. Mapowania wymagają wpisu
  w `claude_profile_files` dla każdego nowego pliku (np. nowego skilla).
- [ ] **Reguły scalania wg typu pliku (KTD6).** Sprawdzić na prawdziwych
  plikach `.claude.json`, `settings.json` i rejestrach pluginów, czy scalanie
  per klucz daje oczekiwany wynik. Sprawdzić, czy Claude Code odtwarza pluginy
  z rejestru bez cache (limit 1 MiB Secretu).

## 3. Znane problemy spoza U6/U7

- [ ] **Testy jednostkowe `narsor-protocol` nie kompilują się** od `7f79c08`
  (`InputContinuationView`, brak pola `permission_decision` w teście
  `HumanInputDeliveryDto`). Przez to `cargo test --workspace` nie przechodzi.
- [ ] **`cargo fmt --check` zgłasza dwa pliki** spoza tej pracy:
  `crates/narsor-server/src/api/routes/control.rs` i
  `src/tui/task_app/tabs/conversation.rs`.
