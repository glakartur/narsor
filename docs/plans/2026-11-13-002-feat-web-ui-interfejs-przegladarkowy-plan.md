---
title: "feat: Interfejs przeglądarkowy (narsor-web) — prace pozostałe"
type: feat
status: active
date: 2026-11-13
origin: brak dokumentu wymagań; zlecenie z rozmowy („nowy moduł z interfejsem przeglądarkowym, działanie jak TUI, lepsze UI i UX")
---

# feat: Interfejs przeglądarkowy (narsor-web) — prace pozostałe

## Summary

Nowy moduł `crates/narsor-web` daje interfejs w przeglądarce o funkcjach TUI. Aplikacja jest klientem tego samego API (`/control/v1`), co TUI. Serwer podaje ją pod `/ui/` z własnego adresu. Fundament jest zrobiony i sprawdzony na działającym serwerze (patrz „Stan obecny"). Ten plan opisuje prace, które zostały: klient API, magazyn stanu, widoki, formularze z serwera, testy, dokumentacja i wydanie. Rozmiar: Deep (10 jednostek, U3–U12).

Żadna zmiana nie jest zacommitowana. Praca jest na gałęzi `main` w katalogu roboczym.

---

## Stan obecny

### Zrobione (U1, U2)

| Element | Plik | Stan |
|---|---|---|
| Crate z plikami aplikacji wbudowanymi w binarkę (`include_dir`) i trasami `/`, `/ui`, `/ui/`, `/ui/{*path}` | `crates/narsor-web/src/lib.rs` | gotowe, 5 testów |
| Nagłówki odpowiedzi: CSP (`script-src 'self'`, `style-src 'self'`, `connect-src 'self'`, `frame-ancestors 'none'`), `nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-cache` | `crates/narsor-web/src/lib.rs` | gotowe |
| Trasy aplikacji dołączone poza bramką właściciela | `crates/narsor-server/src/api/routes.rs` (koniec `router`) | gotowe |
| `ApiClient::browser_url()`: adres `…/ui/#token=…` | `crates/narsor-api-client/src/api/client.rs` | gotowe |
| Komenda `narsor-cli web [--no-open]`. Token nie jest argumentem procesu: `xdg-open` dostaje stronę startową `web-open.html` (tryb 0600) w katalogu stanu | `src/cli/web.rs`, `src/cli/mod.rs`, `src/cli/remote.rs` (`client` jest teraz `pub(super)`) | gotowe, 2 testy |
| Biblioteka Preact + htm, jeden plik, bez Node przy budowaniu | `crates/narsor-web/ui/vendor/preact-htm.js` | dołączona (brak pliku licencji, patrz U12) |
| Reguły wyświetlania z TUI jako czyste funkcje: ikona stanu, liczniki, wieki, tekst kolejki, sloty, PR, filtr, liczba mnoga | `crates/narsor-web/ui/js/model.js` | gotowe, 15 testów Node (`node --test crates/narsor-web/ui-tests/*.mjs`) |

Sprawdzone ręcznie na serwerze testowym: `GET /ui/` zwraca 200 z `text/html`, a `GET /control/v1/tasks` odpowiada na token z adresu.

### Zrobione (U3–U5, sesja 2)

| Element | Plik | Stan |
|---|---|---|
| Klient API: wszystkie trasy TUI, `ApiError` z `kind`, osłona ciała błędu, strumień SSE przez `fetch` | `ui/js/api.js` | gotowe, 11 testów |
| Magazyn stanu: odświeżanie 10 s / 400 ms / po zapisie, ponowienia 1–10 s, strumień z ponowieniem 250 ms × 2ⁿ, limity użycia co 15 s, stany połączenia, blokada zmian, przeniesienie wyboru, powiadomienia 6 s | `ui/js/store.js` | gotowe, 9 testów |
| Teksty: nazwy harnessów, limity użycia, tekst harnessu | `ui/js/text.js` | gotowe, 2 testy |
| Start: token z `#token=` → `sessionStorage`, fragment usunięty; ekran wklejenia tokenu; 401/403 → ekran logowania z powodem; wylogowanie | `ui/js/main.js`, `ui/js/ui/signin.js` | gotowe |
| Powłoka, nagłówek (połączenie, harness, sloty, `?`, `!`, limity), lista (filtr, ukryj ✓, następne czekające, ▲▼ w kolejce), szczegóły — tylko nagłówek, powiadomienie, pomoc, skróty `j k g G Space / z Enter Esc K J R ?` | `ui/js/ui/*.js`, `ui/styles.css`, `ui/index.html` | gotowe; zrzuty 1280 px i 400 px sprawdzone na serwerze testowym |

Decyzja Artura (2026-11-13): w pierwszej wersji token wkleja się w przeglądarce, pełne OIDC nie jest potrzebne. Interfejs jest dla wygody części użytkowników.

### Zrobione (U6 i część U11, sesja 3)

| Element | Plik | Stan |
|---|---|---|
| Ekran logowania: logo, instrukcja „Skąd wziąć token?" (serwer lokalny i zdalny); ikona w nagłówku i karcie | `ui/js/ui/signin.js`, `ui/img/` (kopie z `docs/assets/`) | gotowe |
| Nagłówek szczegółów: workspace, wiersz statusu (A.4, części sortowane po roli), wiersz użycia, komunikat | `ui/js/ui/details.js` | gotowe |
| Rozmowa: wszystkie próby `recorded`, stronicowanie, pamięć podręczna prób zamkniętych, rodzaje `ChatElement`, grupy subagentów, widok surowy, podążanie za końcem | `ui/js/ui/tabs/conversation.js` | gotowe; **nie sprawdzone na kroku agenta** (workflow `template` nie ma agenta) |
| Przebieg: oś pozycji, wybór kroku z rozmową, podążanie | `tabs/progress.js` | gotowe, sprawdzone |
| Zdarzenia: tabela, stronicowanie `after`/`has_more`, teksty z `event_text.rs` | `tabs/events.js`, `text.js` | gotowe, sprawdzone |
| Dyrygent: status, błąd kroku, propozycje (potwierdź/odrzuć), decyzje (cofnij), czat i wysyłanie | `tabs/conductor.js` | gotowe; akcje nie sprawdzone na żywym dyrygencie |
| Info: pięć sekcji | `tabs/info.js` | gotowe, sprawdzone |
| Adres `#/task/<id>/<zakładka>`, skróty `[` `]` | `ui/js/ui/app.js` | gotowe |
| `build.rs` z `rerun-if-changed=ui` (wcześniej zmiana w `ui/` nie przebudowywała crate) | `crates/narsor-web/build.rs` | gotowe |

Testy Node: 41. Zrzuty wszystkich zakładek: skrypt CDP w scratchpadzie sesji (Node 22, bez zależności).

### Zrobione (U7, U8, sesja 4)

| Element | Plik | Stan |
|---|---|---|
| Reguły panelu: `deliver` (wszystkie plany), sprawdzanie pól jak w TUI, `resolveSend` z `{{pole}}`, lista akcji | `ui/js/panel.js` | gotowe, 11 testów |
| Panel „Czeka na Ciebie": przełącznik formularzy, linie kontekstu z tonem, pozycje (zalecane, ↩ wraca do), pola, szkic per formularz, czat (`comment`), formularz akcji otwiera panel akcji | `ui/js/ui/answer.js`, `fields.js` | gotowe; odpowiedź `finish` wysłana z przeglądarki, serwer przyjął (202, input `queued`) |
| Panel akcji: wybór z podmenu, pola, potwierdzenie (`! nieodwracalne`), „Otwórz link" PR, skrót `.` | `ui/js/ui/actions.js` | gotowe; wysłanie akcji nie sprawdzone na serwerze |
| Ostrzeżenie przed zamknięciem karty z niewysłanym tekstem (`beforeunload`) | `ui/js/ui/app.js` | gotowe |

Testy Node: 52.

Nie zrobione w U7: panele własne bez `panel` (propozycja dyrygenta, nieudany krok z „Uruchom ponownie z notatką", surowy błąd provisioningu). Propozycje i błąd kroku są w zakładce Dyrygent. Adnotacja dyrygenta `◆` nie jest dodana.

### Zrobione (U9, U10, sesja 5)

| Element | Plik | Stan |
|---|---|---|
| Reguły nowego zadania: kolejność pól jak w TUI (Ticket 15 zn., Tytuł 50 zn., repozytorium + gałąź per workspace, wejścia), typowanie wejść, domyślne wartości | `ui/js/newtask.js` | gotowe, 5 testów |
| Okno „Nowe zadanie": katalog workflow (niepoprawne z naruszeniami), podpowiedzi gałęzi z serwera, `ścieżka…` przez `validate` i `POST /tasks`, własny `task_id` i „Ponów" po utraconej odpowiedzi, szkic po zamknięciu okna, skrót `n` | `ui/js/ui/newtask.js` | gotowe; zadanie z ticketem i tytułem utworzone z przeglądarki |
| Historia (zarchiwizowane zadania z Info; stare wykonania z „Odzyskaj"), Repozytoria (lista, dodanie, usunięcie z potwierdzeniem), domyślny harness, limit zadań ±1; skróty `H` `R` `h` `L`; odświeżanie ręczne przeniesione na `u` | `ui/js/ui/settings.js` | gotowe; limit 1→2 zmieniony z przeglądarki |

Testy Node: 57.

Różnice wobec planu: wiersz „wysyłanie… / nie wysłano" jest w oknie nowego zadania, nie na liście. Szczegóły starego wykonania (Przebieg, Rozmowa, Zdarzenia, Dyrygent z tras H3–H7) nie są zrobione — historia pokazuje listę i „Odzyskaj".

### Zrobione (U12 i część U11, sesja 6)

| Element | Plik | Stan |
|---|---|---|
| Test integracyjny: `/ui/` bez tokenu (200, CSP bez `unsafe-inline`), moduł JS, przekierowanie `/`, `/control/v1/tasks` bez tokenu 401, z tokenem 200 | `tests/integration/web_ui.rs` | gotowe, 2 testy |
| Licencje: komentarz w pliku biblioteki, `VENDOR-LICENSES.md` (Preact MIT, htm Apache-2.0) | `crates/narsor-web/` | gotowe; wersja Preact podana jako „10.x z htm 3.1.1" — plik nie zawiera numeru |
| Dokumentacja: `docs/user/web.md`, `docs/README.md`, `README.md`, komenda `web` w `docs/user/cli.md`, D36 w `docs/dev/architecture.md`, crate i testy Node w `docs/dev/building-and-testing.md` | `docs/` | gotowe |
| Wydanie: `dist/package-linux.sh` i CI budują `--workspace`, pliki są w binarce — bez zmian | — | sprawdzone |
| Długa rozmowa: ostatnie 200 pozycji na próbę, „pokaż wcześniejsze" | `tabs/conversation.js` | gotowe |

### Zmiana architektury (sesja 7, decyzja Artura 2026-11-14)

Interfejs nie jest już częścią `narsor-server`, a `narsor-cli web` nie istnieje. Powód: `narsor-cli` i TUI są tylko klientami API; serwer wystawia tylko API.

| Element | Plik | Stan |
|---|---|---|
| Binarka `narsor-web`: serwer HTTP plików interfejsu i proxy API do `narsor-server` | `crates/narsor-web/src/main.rs`, `gateway.rs` | gotowe |
| Ustawienia `web_bind` (domyślnie `127.0.0.1:8484`, tylko loopback do czasu TLS), `web_upstream` (`local` albo URL), `web_token` (`server` albo `passthrough`) | `narsor-shared` `config/settings.rs`, `file.rs`; `crates/narsor-web/src/settings.rs` | gotowe, 4 testy |
| Tryb `server`: token sesji przy każdym starcie (plik `web-session` 0600, `--open`), token serwera tylko w `narsor-web`, sprawdzanie `Host`/`Origin`, tylko trasy interfejsu | `gateway.rs` | gotowe |
| `web_upstream: local`: lokalny daemon znajdowany albo uruchamiany; po restarcie daemona ponowne połączenie | `main.rs` | sprawdzone ręcznie |
| `ApiClient::forward` (przekazanie żądania, ponowienie po 401); usunięte `browser_url` | `narsor-api-client` | gotowe |
| Ekran logowania według trybu (`GET /gateway/v1/info`) | `ui/js/ui/signin.js` | gotowe |
| Test integracyjny: serwer + `narsor-web` w obu trybach | `tests/integration/web_ui.rs` | 5 testów |
| Wydanie: `narsor-web` w `package-linux.sh`, `install.sh` (role `local`, `frontend`, `server --with-frontend`), smoke-test CI i release | `dist/`, `.github/workflows/` | gotowe |
| Dokumentacja: `web.md`, `configuration.md`, `installation.md`, D36, `building-and-testing.md`; komenda `web` usunięta z `cli.md` | `docs/` | gotowe |

Odłożone: TLS w `narsor-web`, funkcje lokalne (`/local/v1`: klonowanie, IDE).

### Nie zrobione

- Test trybu `network` (reguła zaufanego proxy dla `/ui/`).
- Przeciąganie w kolejce; pełny przegląd dostępności (strzałki w zakładkach, czytnik ekranu).
- Szczegóły starych wykonań w historii (H3–H7).
- Panele własne bez `panel` (U7).
- Testy Node w CI — decyzja Artura.

### Materiały źródłowe (zapisane w repozytorium)

- `docs/sources/narsor-owner-api-wire-reference.md` — pełny opis API na poziomie JSON: ścieżki, ciała, kody, kształty DTO, strumień SSE, 27 pułapek. Wyprowadzony z atrybutów serde; nie był sprawdzony na działającym serwerze.
- `docs/sources/narsor-tui-inventory-part-a.md` — inwentarz TUI, część A: powłoka, nagłówek, lista, nagłówek szczegółów, stopka, model odświeżania, różnice między `docs/user/tui.md` a kodem.
- Brak części B–D inwentarza (zakładki, panel odpowiedzi, panel akcji, nowe zadanie, historia, repozytoria). Dla nich źródłem jest `docs/user/tui.md` i kod `src/tui/task_app/`.

---

## Requirements

- R1. Interfejs działa w przeglądarce pod adresem serwera, bez osobnej instalacji i bez Node przy budowaniu i wydaniu. (U1, U2 — zrobione)
- R2. Te same funkcje co TUI: kolejka, szczegóły z pięcioma zakładkami, odpowiedź na to, co czeka, akcje, nowe zadanie, historia, repozytoria, domyślny harness, limit zadań, limity użycia. (U5–U10)
- R3. Te same reguły stanu co TUI: ikona, słowo, liczniki `?` i `!`, kolejność listy z serwera. (U2 — zrobione; U5 używa)
- R4. Akcje i formularze pochodzą z serwera (`TaskSummary.actions`, `TaskSummary.panel`). Aplikacja nie ma własnej listy akcji ani `switch` po identyfikatorze akcji. (U7, U8)
- R5. Dane na żywo: strumień zdarzeń, okresowy odczyt, stan połączenia, blokada zmian bez połączenia. (U4)
- R6. Każde wywołanie niesie `Authorization: Bearer`. Token nie trafia do adresu wysyłanego na serwer, do logów ani do argumentów procesu. (U1 — zrobione; U3)
- R7. Lepsze UI i UX niż w terminalu: mysz i klawiatura, jasny i ciemny motyw, układ na wąski ekran, czytelna rozmowa z agentem. (U5–U11)
- R8. Tekst agenta, runnera i workflow jest zawsze traktowany jako tekst, nigdy jako HTML. (U6)

---

## Scope Boundaries

- Bez zmian w silniku, protokole i regułach serwera. Jedyna zmiana serwera to trasy `/ui/`.
- Bez logowania OIDC w przeglądarce (przekierowanie do dostawcy tożsamości). W trybie `network` operator wkleja token albo używa `narsor-cli --server … web`.
- Bez materiałów (wysyłanie plików do zadania). TUI też ich nie obsługuje.
- Bez tras kontrolerów (`/control/v1/controllers`, enrollments) i bez `default-backend`. TUI ich nie używa.
- Bez tłumaczenia na inne języki. Etykiety są po polsku, jak w TUI.
- Bez narzędzi Node w budowaniu. Testy Node są dodatkowe i lokalne.

### Deferred to Follow-Up Work

- Logowanie OIDC z PKCE w przeglądarce i odświeżanie tokenu.
- Wysyłanie i przegląd materiałów zadania.
- Powiadomienia przeglądarki (Notification API), gdy zadanie zaczyna czekać na operatora.
- Wspólne źródło reguł wyświetlania dla TUI i przeglądarki (dziś reguły są powtórzone w `model.js`).

---

## Key Technical Decisions

- **Aplikacja w binarce serwera, pod tym samym adresem co API.** Brak CORS, jedna instalacja, jedna wersja. Odrzucone: osobny serwer plików (druga usługa i CORS), osobny proces `narsor-web` (drugi port i token).
- **ES modules bez kroku budowania, Preact + htm jako jeden plik w repozytorium.** `cargo build` i `dist/package-linux.sh` nie potrzebują Node. Odrzucone: React + Vite + TypeScript (Node w CI i w wydaniu), czysty DOM bez biblioteki (dużo kodu na odświeżanie widoków).
- **Token we fragmencie adresu, potem w `sessionStorage` karty.** Przeglądarka nie wysyła fragmentu na serwer. Aplikacja usuwa fragment z paska adresu zaraz po starcie (`history.replaceState`).
- **Adres API względny do `/ui/`** (`new URL('..', location.href)`), bo serwer może stać za proxy z prefiksem ścieżki.
- **Strumień SSE przez `fetch`, nie `EventSource`.** `EventSource` nie wysyła nagłówka `Authorization`.
- **Zdarzenie strumienia tylko zleca odczyt listy (z opóźnieniem 400 ms).** Aplikacja nie łata wierszy z treści zdarzeń. To jest prostsze niż w TUI i daje ten sam stan po krótkim opóźnieniu. Wyjątek do rozważenia: zdarzenie `moved` można zastosować od razu.
- **Czyste reguły w osobnych modułach z testami Node** (`model.js`, `panel.js`, `text.js`). Widoki tylko rysują.
- **Ścisłe CSP bez `'unsafe-inline'`.** Style są w pliku `styles.css`. Preact ustawia style przez CSSOM, co CSP dopuszcza. W kodzie nie wolno używać `innerHTML` ani `dangerouslySetInnerHTML`.

---

## High-Level Technical Design

Układ plików w `crates/narsor-web/ui/`:

```text
index.html            powłoka dokumentu, <script type="module" src="js/main.js">
styles.css            zmienne motywu (jasny i ciemny), układ, role kolorów
vendor/preact-htm.js  biblioteka
js/lib.js             eksport biblioteki
js/model.js           reguły stanu zadania                (zrobione)
js/panel.js           wpis panelu → delivery; akcja → polecenie   (czyste, testy)
js/text.js            tekst zdarzeń, wiersz użycia, limity        (czyste, testy)
js/api.js             klient HTTP, błędy, strumień SSE
js/store.js           stan aplikacji, odświeżanie, połączenie, powiadomienia
js/main.js            start: token, logowanie, render
js/ui/*.js            widoki
```

Przepływ danych:

```text
narsor-cli web ──▶ /ui/#token=…  ──▶ main.js: token → sessionStorage, fragment usunięty
                                        │
                     store.js ◀── api.js ── GET /control/v1/tasks?archived=false  (start, co 10 s, po zdarzeniu +400 ms)
                        │           └────── GET /control/v1/events/stream?cursor=…  (fetch, ponawianie 250 ms × 2ⁿ, max 15 s)
                        ▼
                   widoki (Preact) ──▶ akcje operatora ──▶ api.js (POST z idempotency_key i expected_generation)
```

Układ ekranu: nagłówek (połączenie, harness, sloty, liczniki, limity użycia), po lewej kolejka zadań, po prawej szczegóły: nagłówek zadania, zakładki, na dole panel „Czeka na Ciebie" albo panel akcji. Poniżej 900 px szerokości jeden panel naraz.

---

## Implementation Units

### U3. Klient API i strumień zdarzeń

**Pliki:** `ui/js/api.js`, `ui/js/lib.js`, `ui-tests/api.test.mjs`.

- Klasa `Api` z adresem bazowym i tokenem. Jedna metoda `request(method, path, {query, body, timeoutMs, signal})`.
- Błąd `ApiError {status, code, message, kind}`; `kind`: `unauthorized` (401), `forbidden` (403), `not_found` (404), `request` (400/415/422), `refused` (409), `degraded` (503), `server` (5xx), `unavailable` (brak sieci, limit czasu). Ciało błędu może być JSON `{code,message}`, zwykły tekst albo puste — nigdy `response.json()` bez osłony.
- Kody 200, 201 i 202 to sukces.
- Metody dla wszystkich tras z kolumną „TUI = yes" w `docs/sources/narsor-owner-api-wire-reference.md` (sekcja 2), oraz `whoami`.
- Odczyty przekazywane przez runner (rozmowa, zdarzenia, dyrygent) mają limit czasu 25 s i dają się przerwać (`AbortController`).
- `streamEvents(cursor, signal)`: generator asynchroniczny kopert `{cursor, event}`. Ramki dzielone na `\n\n`, czytane tylko linie `data: `, komentarze `:` pominięte, kursor kodowany w adresie.
- `idempotencyKey()` = `crypto.randomUUID()`.

**Testy:** parser ramek SSE (ramka w dwóch kawałkach, komentarz, kilka ramek naraz), mapowanie statusów na `kind`, ciało błędu jako tekst i puste.

### U4. Stan aplikacji, odświeżanie i połączenie

**Pliki:** `ui/js/store.js`, `ui/js/main.js`, `ui/js/ui/signin.js`, `ui-tests/store.test.mjs`.

- Start: odczyt tokenu z fragmentu → `sessionStorage` → usunięcie fragmentu. Brak tokenu albo 401 → ekran logowania z polem na token. 403 → komunikat „to konto nie jest właścicielem instalacji".
- Magazyn: `tasks`, `cursor`, `selectedTaskId`, `filter`, `hideDone`, `tab`, `connection`, `harness`, `capabilities`, `usage`, `notification`, szkice per zadanie. Subskrypcja przez hook `useStore(selector)`.
- Odświeżanie jak w części A, sekcja A.8: lista + harness na starcie, co 10 s, 400 ms po zdarzeniu, po każdym zapisie; ponawianie 1, 2, 4, 8, 10 s. Limity użycia co 15 s.
- Stany połączenia i teksty jak w A.2: `łączę z …`, `live`, `ponawiam (Ns)`, `brak połączenia`, „stan sprzed …".
- Blokada zmian bez połączenia z tekstem z A.2 („akcja zmieniająca stan jest zablokowana").
- Wybór trzyma `task_id`. Gdy zadanie znika, wybór przechodzi na sąsiada (reguła z A.3) i pokazuje się ostrzeżenie.
- Powiadomienia: jedno miejsce, 6 s, rodzaje sukces / ostrzeżenie / błąd.
- Odczyty szczegółów tylko dla wybranego zadania i widocznej zakładki; stają się należne, gdy zmienia się `revision`.

**Testy:** harmonogram odczytów i ponowień (zegar podany z zewnątrz), przeniesienie wyboru, blokada zmian.

### U5. Powłoka, nagłówek, lista zadań, style

**Pliki:** `ui/index.html`, `ui/styles.css`, `ui/js/ui/app.js`, `header.js`, `tasklist.js`, `toast.js`, `help.js`.

- Nagłówek: połączenie, harness (`(konfiguracja)` gdy brak wyboru operatora), sloty, liczniki `?` i `!`, limity użycia (format z A.6, kolory wg `status`).
- Lista: wiersz z ikoną, identyfikatorem, tytułem, etykietą (`#n` albo krok z ikoną) i wiekiem. Kolejność z serwera. Filtr tekstowy, przełącznik „ukryj zakończone" z licznikiem, przycisk „następne czekające".
- Przesuwanie w kolejce: przyciski góra / dół oraz przeciąganie; tylko dla `runner_state == queued`; `POST …/move` z `expected_generation`.
- Motyw jasny i ciemny z `prefers-color-scheme`; role kolorów: attention, problem, live, calm, done, focus, conductor. Paleta z logo (`docs/assets/narsor-logo.svg`: indygo `#818cf8`, bursztyn `#f59e0b`).
- Skróty klawiszowe jak w TUI tam, gdzie mają sens w przeglądarce: `j`/`k`, `g`/`G`, `/`, `Space`, `Enter`, `[`/`]`, `.`, `n`, `z`, `H`, `R`, `h`, `L`, `?`. Skróty nie działają, gdy fokus jest w polu tekstowym.
- Stany puste: `wczytuję zadania…`, `brak zadań`, `brak danych`.

**Sprawdzenie:** zrzuty ekranu na serwerze testowym w obu motywach i przy szerokości 400 px.

### U6. Szczegóły zadania: nagłówek i pięć zakładek

**Pliki:** `ui/js/ui/details.js`, `tabs/conversation.js`, `tabs/progress.js`, `tabs/events.js`, `tabs/conductor.js`, `tabs/info.js`, `ui/js/text.js`, `ui-tests/text.test.mjs`.

- Nagłówek: wiersze workspace (repo, gałęzie, `+N −M`, link PR tylko dla bezpiecznego adresu), wiersz statusu (A.4), wiersz użycia (`razem $… · ✨ … · ⚡ …`), komunikat o utracie albo niedostępnym harnessie. Komunikaty „jeszcze nie wystartowało" i „storage usunięty przy archiwizacji".
- Kolejność zakładek jak w kodzie TUI: Rozmowa, Przebieg, Zdarzenia, Dyrygent, Info. Domyślna: Rozmowa.
- **Rozmowa:** wszystkie próby pozycji z `conversation.state == recorded`, stronicowanie `from` / `next_entry` (reguła zatrzymania z opisu API, T12). Elementy `ChatElement` wg `kind`: prompt, wiadomość, tekst agenta, myślenie (zwinięte), wywołanie narzędzia z wynikiem (zwinięte, błąd widoczny), pytanie agenta, wynik biegu, techniczne (zwinięte). Wpisy jednego subagenta jako jedna zwijana grupa z liczbą wpisów i błędów. Podążanie za końcem. Widok surowy wpisu (`raw_base64`).
- **Przebieg:** oś pozycji z `Timeline.positions`: glif statusu (`✓ ✗ ● ?`), ikona kroku, nazwa, start, czas, koszt, tokeny, subagenci; drugi wiersz z modelem, effortem i harnessem. Wybór kroku pokazuje jego rozmowę (ten sam komponent co Rozmowa, zawężony do prób pozycji). Podążanie za bieżącym krokiem.
- **Zdarzenia:** tabela (numer, czas, typ, szczegół) z `execution-events`, stronicowanie `after` / `has_more`. Teksty jak w `src/tui/event_text.rs`. Odpowiedź `unavailable` to komunikat, nie błąd.
- **Dyrygent:** status (harness, co robi teraz), czat, propozycje (potwierdź / odrzuć), decyzje (cofnij, gdy `undoable`), pole wiadomości. Wszystkie akcje przez `POST …/operator-commands`.
- **Info:** sekcje Zapieczętowane wejścia, Workflow, Workspace, Runner, Kontroler (etykiety z `src/tui/task_app/tabs/info.rs`).
- Konflikt 409 `conversation_reader_starting` → ponowienie po krótkiej przerwie. 503 → tekst „runner nie odpowiada — spróbuję ponownie przy następnej zmianie".

**Testy:** tekst każdego rodzaju zdarzenia, wiersz użycia (przykład z A.4), grupowanie subagentów, reguła stronicowania rozmowy.

### U7. Panel „Czeka na Ciebie" (formularze z serwera)

**Pliki:** `ui/js/panel.js`, `ui/js/ui/answer.js`, `ui-tests/panel.test.mjs`.

- `panel.js` powtarza `narsor_protocol::panel::deliver` (`crates/narsor-protocol/src/panel/mod.rs`): plan wpisu → `HumanInputDeliveryDto`, zawsze z `episode`. `PanelPlan` i `PanelFieldKind` są tagowane zewnętrznie (napis albo obiekt z jednym kluczem). Puste pole nieobowiązkowe jest pomijane.
- Formularze z `task.panel.forms`; otwiera się `panel.default`. Przełącznik formularzy, gdy jest ich kilka. Każdy formularz ma własny szkic, który przeżywa odświeżenie.
- Rodzaje: `answer`, `permission`, `entry_limit` i `action` z `waiting_id` — wpisy z `definition.entries` (etykieta, opis, pola, oznaczenie „zalecane", „↩ wraca do …"), linie kontekstu z tonem. `chat` — pole wiadomości, `POST …/comment`. `action` bez `waiting_id` — otwiera panel akcji (U8).
- Wysłanie: `POST …/input` z `pending_input_id = definition.episode` i nowym `idempotency_key`. Po sukcesie powiadomienie z `entry.summary`, pole wyczyszczone. Po błędzie tekst zostaje.
- Zadania z własnym panelem (`panel` nieobecny przy propozycji dyrygenta, nieudanym kroku, nieudanym provisioningu, problemie runnera): propozycje dyrygenta z potwierdzeniem i odrzuceniem, nieudany krok z diagnozą i „Uruchom ponownie z notatką" (`restart_step` z `failure_key`), surowy błąd. Źródło reguł: `src/tui/task_app/panel/legacy.rs`, `src/tui/failure_form.rs`.

**Testy:** każdy plan z tabeli w opisie API (sekcja 5.3), w tym `output` z `artifacts.*`, `variables.*`, liniami `klucz=wartość` i błędem podwójnego klucza; wzorzec uprawnienia pusty albo z nawiasem.

**Otwarte:** dokładne reguły paneli własnych wymagają przeczytania `panel/legacy.rs` i `panel/state.rs` (nie były czytane w całości).

### U8. Panel akcji

**Pliki:** `ui/js/ui/actions.js`, rozszerzenie `ui/js/panel.js`.

- Lista z `task.actions` (tylko `surface == panel`), w kolejności `order`. Brak pola `actions` → „Brak danych z serwera". Pusta lista → „brak dostępnych akcji".
- Trzy kroki jak w TUI: wybór (podmenu z `children` w tym samym panelu) → pola (`fields`; krok pominięty, gdy brak pól) → potwierdzenie (nazwa, opis, wpisane wartości, `! nieodwracalne` dla `irreversible`).
- `resolveSend`: zamiana `{{pole}}` w `send.command` (tylko napis równy dokładnie `{{nazwa}}`; puste pole → `null`); `owner` → `POST …/commands`, `operator` → `POST …/operator-commands`, `comment` → `POST …/comment`. Nieznany `type` → „ta akcja wymaga nowszej wersji klienta".
- Każde wysłanie niesie `expected_generation`. 409 → powiadomienie z powodem i ponowny odczyt listy.
- „Otwórz link" na końcu, gdy zadanie ma bezpieczny adres PR.

**Testy:** `resolveSend` dla trzech typów, zagnieżdżone zamiany, puste pole komentarza jako błąd.

### U9. Nowe zadanie

**Pliki:** `ui/js/ui/newtask.js`.

- Odczyty: `capabilities`, `workflows`, `repositories`. Katalog workflow ze źródłem i naruszeniami (`valid == false` nie da się wybrać).
- Formularz z `inputs` wybranego workflow: `boolean` jako wybór, `integer` sprawdzany, wymagane pole zgłaszane przy polu. Dla każdego workspace wybór repozytorium i pole gałęzi z podpowiedziami z `GET …/repositories/{id}/branches` (domyślna gałąź repozytorium wstępnie wybrana; można wpisać inną).
- Wysłanie: `POST /control/v1/tasks` z własnym `task_id` (`task-<uuid bez myślników>`), żeby ponowienie było idempotentne. Wiersz „wysyłanie…" na końcu listy do czasu, gdy serwer pokaże zadanie; „nie wysłano" z przyciskiem ponowienia.
- Ścieżka definicji na dysku serwera: tylko gdy `capabilities.workflow_paths`; najpierw `POST …/workflows/validate`, potem `POST /tasks`.
- Szkic formularza przeżywa zamknięcie okna dialogowego.

**Otwarte:** `src/tui/task_app/new_task/mod.rs` (1481 linii) nie był czytany; reguły pól ticket i tytuł (limit 50 znaków) i kolejność pól trzeba z niego wziąć.

### U10. Historia, repozytoria, harness, limit zadań

**Pliki:** `ui/js/ui/history.js`, `repositories.js`, `settings.js`.

- Historia: zadania zarchiwizowane (`tasks?archived=true`) i stare wykonania (`/control/v1/executions`, gdy `capabilities.legacy_executions`). Szczegóły starego wykonania: Przebieg, Rozmowa, Zdarzenia, Dyrygent z tras H3–H7 (kształt dyrygenta jest inny niż dla zadania). „Odzyskaj" dla `recoverable` (`POST …/recover` z komentarzem).
- Repozytoria: lista, dodanie (`id`, `remote`, opcjonalnie nazwa, dostawca, domyślna gałąź, opis), usunięcie z potwierdzeniem; wpisy `unreadable` pokazane.
- Domyślny harness: lista z `supported`, pierwsza pozycja „domyślny z konfiguracji (<nazwa>)" (DELETE), bieżący oznaczony; nagłówek pokazuje `harness <nowy>…` do odpowiedzi.
- Limit zadań: bieżące liczby (`limit`, `slots_held`, `overshoot`, `waiting_to_stop`), przyciski +1 i −1 (`PUT …/concurrency-limit`).

### U11. Dostępność i dopracowanie UX

- Pełna obsługa z klawiatury, widoczny fokus, role ARIA dla listy, zakładek i okien dialogowych, `aria-live` dla powiadomień.
- Stan nie zależy tylko od koloru: zawsze ikona i słowo.
- Adres strony odzwierciedla wybór (`#/task/<id>/<zakładka>`), żeby dało się odświeżyć stronę i wrócić w to samo miejsce. Token nigdy nie jest w tym adresie.
- Zabezpieczenie przed utratą szkicu przy zamknięciu karty (`beforeunload`, tylko gdy jest niewysłany tekst).
- Długie rozmowy: rysowanie tylko widocznej części albo stronicowanie „pokaż wcześniejsze", żeby karta nie zwalniała.

### U12. Testy, dokumentacja, wydanie

- Test integracyjny serwera: `GET /ui/` bez tokenu zwraca 200 w trybie zaufanym; `/control/v1/tasks` bez tokenu nadal zwraca 401; w trybie `network` `/ui/` przechodzi przez regułę zaufanego proxy. Miejsce: `tests/integration/` (wzorzec z istniejących testów API).
- Test `narsor-cli web --no-open` (adres ma `/ui/#token=`).
- `crates/narsor-web/ui/vendor/LICENSES.md`: Preact (MIT), htm (Apache-2.0), z wersjami (preact 10.x z paczki `htm@3.1.1/preact/standalone`).
- Dokumentacja: `docs/user/web.md` (uruchomienie, logowanie, skróty, różnice wobec TUI), wpis w `docs/README.md`, punkt w „Features" w `README.md`, komenda `web` w `docs/user/cli.md`, decyzja w `docs/dev/architecture.md` (D36 odkładała Web UI — zmienić), opis crate w `docs/dev/building-and-testing.md` razem z komendą testów Node.
- Wydanie: `dist/package-linux.sh` i `.github/workflows/` — sprawdzić, że nowy crate nie wymaga zmian (pliki są w binarce `narsor-server`). Rola instalatora „frontend" nie ma serwera: `narsor-cli --server … web` otwiera interfejs zdalnego serwera.
- `cargo fmt`, `cargo clippy --workspace`, `cargo test --workspace`.

---

## Kolejność i zależności

```text
U3 ─▶ U4 ─▶ U5 ─▶ U6 ─▶ U7 ─▶ U8 ─▶ U9 ─▶ U10 ─▶ U11 ─▶ U12
                  │             ▲
                  └── text.js ──┘ (panel.js dla U7 i U8 można pisać równolegle z U5–U6)
```

Po U5 aplikacja pokazuje kolejkę na żywo (pierwszy użyteczny stan). Po U8 da się prowadzić zadanie od początku do końca bez TUI. Każda jednostka nadaje się na osobny PR.

---

## System-Wide Impact

- `narsor-server` rośnie o rozmiar plików aplikacji (dziś ok. 13 kB biblioteki; docelowo szacunkowo 150–250 kB tekstu).
- Serwer odpowiada teraz na `GET /` przekierowaniem do `/ui/` (wcześniej 404). Żaden klient nie używał `/`.
- Nowa zależność: `include_dir 0.7` (zmiana w `Cargo.lock`).
- Nowy plik w katalogu stanu: `web-open.html` (0600), nadpisywany przy każdym `narsor-cli web`. Zawiera token do czasu następnego uruchomienia serwera.
- Powierzchnia ataku: przeglądarka trzyma token właściciela. Chronią go CSP bez skryptów zewnętrznych i bez `inline`, brak `innerHTML`, brak referrera i brak CORS.

---

## Risks & Dependencies

| Ryzyko | Skutek | Ograniczenie |
|---|---|---|
| Opis API nie był sprawdzony na działającym serwerze | błędne kształty JSON w kliencie | w U3 sprawdzić każdą trasę `curl`-em na serwerze testowym z prawdziwym zadaniem (workflow ze skryptem i krokiem `human`) |
| Reguły powtórzone w JS rozejdą się z TUI (`model.js`, `panel.js`) | dwa interfejsy mówią co innego | testy Node z przypadkami z testów Rust; follow-up: wspólne źródło |
| Brak testów Node w CI | regresja reguł bez sygnału | udokumentować komendę; rozważyć krok CI (Node jest na runnerach GitHub) |
| Token w `sessionStorage` | XSS odczyta token | ścisłe CSP, tekst tylko przez węzły tekstowe, przegląd bezpieczeństwa przed wydaniem |
| Tryb `network`: token OIDC wygasa, strumień się kończy | operator musi wkleić token ponownie | ekran logowania po 401 zachowuje stan widoku; follow-up: OIDC z PKCE |
| Długie rozmowy (tysiące wpisów) | wolna karta | U11: rysowanie części widocznej |
| Część B–D inwentarza TUI nie powstała | braki funkcji względem TUI | przed U6–U10 przeczytać wskazane pliki TUI; `docs/user/tui.md` jako lista kontrolna |

---

## Open Questions

### Do decyzji Artura

- Kto używa interfejsu i w jakiej sytuacji TUI nie wystarcza? Odpowiedź ustala priorytet: prowadzenie zadań z telefonu, podgląd dla zespołu, czy pełne zastąpienie TUI.
- ~~Czy tryb `network` z wklejanym tokenem wystarcza na pierwsze wydanie?~~ Tak (decyzja 2026-11-13).
- Czy testy Node mają wejść do CI? (krok `node --test crates/narsor-web/ui-tests/*.mjs`; Node jest na runnerach GitHub)
- Czy pliki w `docs/sources/` (opis API, inwentarz TUI) mają zostać w repozytorium?

### Deferred to Implementation

- Czy zdarzenie `moved` stosować od razu, czy czekać na odczyt listy.
- Dokładny wygląd osi „Przebieg" na wąskim ekranie.
- Czy adnotacja dyrygenta do bramki (`◆ dyrygent: …`, `src/tui/task_app/panel/input.rs` `annotate`) wchodzi do U7.

---

## Sprzątanie po sesji

- W tle działa serwer testowy na katalogu roboczym sesji (`…/scratchpad/root`). Nie dotyka `~/.narsor`. Zatrzymanie: `narsor-server --root <ten katalog> stop`.
- Zmiany nie są zacommitowane. Przed commitem utworzyć gałąź (np. `feat/web-ui`); `.gitignore` i `deploy/kind/` były zmienione wcześniej i nie należą do tej pracy.
