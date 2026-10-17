# Prompt dla Claude Code: dokończ edycję profilu Claude w Kubernetesie

Pracujesz w repozytorium Narsor. Zrealizuj pełny przepływ edycji konfiguracji Claude Code opisany niżej. Przed zmianami przeczytaj plan `docs/plans/2026-10-17-1520-feat-deferred-harness-permissions-plan.md`, zwłaszcza kontrakt produktu R11–R17, przepływ F8, KTD6–KTD7 i jednostkę U7.

## Wymagania użytkownika

Są dwa sposoby zmiany Secretu z profilem Claude Code:

1. Ktoś zmienia Secret poza Narsorem. Narsor przyjmuje tę zmianę jako aktualną prawdę. Nie odrzuca jej, nie cofa i nie przywraca starej zawartości.
2. Użytkownik rozpoczyna zmianę przez Narsor. Narsor przygotowuje zapisywalną kopię plików z zamontowanego Secretu, uruchamia Claude Code z tą kopią, użytkownik edytuje konfigurację, a po zakończeniu Narsor zbiera pliki i publikuje je z powrotem do Secretu Kubernetes.

Secret profilu musi być dostępny w każdym podzie uruchamiającym Claude Code — również w runnerach zwykłych zadań, bo bez niego harness nie może działać. Montaż jest tylko do odczytu. Claude Code nie dostaje uprawnień Kubernetes i sam nie zapisuje Secretu. Narsor/kontroler wykonuje publikację.

Konfiguracja publikowana do Secretu ma obejmować co najmniej uwierzytelnianie, pluginy, skills, MCP, reguły uprawnień i uprawnienia domyślne. Można opublikować całość konfiguracji Claude Code, jeśli nie obejmuje historii sesji ani stanu konkretnego taska. Uwzględnij wszystkie pliki i ścieżki skonfigurowane przez `claude_profile_files`.

Jeśli ktoś zaktualizuje Secret w czasie edycji, Narsor nie może nadpisać tej zmiany starą kopią. Zachowaj zewnętrzną zawartość i odśwież/rebazuj publikację albo pokaż użytkownikowi czytelny konflikt. Nie dodawaj drugiej akceptacji po zakończeniu edycji użytkownika; samo rozpoczęcie edycji w Narsor jest intencją zmiany.

## Stan istniejącego kodu do weryfikacji

Commit `d27ca92` dodał w `crates/narsor-controller/src/kubernetes/manifest.rs` konfigurację `claude_profile_files` w formacie `secret-key=relative/path`, montowanie wskazanych plików Secretu read-only oraz kopiowanie plików do prywatnego profilu w `crates/narsor-runner/src/runner/storage.rs`. Sprawdź tę implementację: w szczególności czy każdy runner Claude Code dostaje Secret, czy mapowania nested paths działają w Kubernetes i czy kopia zachowuje uwierzytelnianie, MCP, pluginy oraz skills.

Aktualny mechanizm aktualizacji kontrolera i protokołu profilu nadal obsługuje pojedynczy `settings.json` i ma stary przepływ „propozycja, a potem akceptacja”. Nie spełnia to powyższych wymagań. Zmień lub zastąp go tak, aby Narsor mógł uruchomić sesję edycji, zebrać pliki profilu i opublikować je do wskazanego Secretu.

## Oczekiwany rezultat

- Znajdź i dodaj wąski, uwierzytelniony sposób rozpoczęcia edycji profilu przez Narsor.
- Uruchom dedykowany Claude Code z zapisywalną kopią bieżącego profilu i standardowym sekretem launchu potrzebnym runnerowi.
- Po zakończeniu zbierz tylko pliki należące do profilu; nie zbieraj historii sesji ani danych taska.
- Zapisz zebrane pliki przez Narsor/controller wyłącznie do skonfigurowanego Secretu. Nie dawaj runnerowi prawa zapisu Secretów.
- Zachowaj zewnętrzne aktualizacje; obsłuż wyścigi bez ich cofania.
- Zaktualizuj protokół, CLI/API, konfigurację K8s, admission policy/RBAC i dokumentację zgodnie z istniejącymi wzorcami repozytorium.
- Dodaj testy przypadków akceptacyjnych U7 i uruchom odpowiednie testy oraz kontrole kompilacji. Nie loguj wartości Secretu.
- Zweryfikuj też zmiany z `d27ca92`, a nie tylko nowy kod. Popraw znalezione błędy.
- Zaktualizuj plan tak, by odpowiadał finalnej implementacji. Użytkownik wcześniej zezwolił na commit zmian razem z planem; utwórz commit(y), jeśli implementacja i weryfikacja są gotowe.

Na końcu podaj krótko: co działa dla obu sposobów zmiany Secretu, jak uruchomić edycję, jakie testy przeszły, numer(y) commita i ograniczenia, które faktycznie pozostały.
