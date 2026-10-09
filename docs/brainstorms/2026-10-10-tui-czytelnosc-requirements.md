---
date: 2026-10-10
topic: tui-czytelnosc
---

# Czytelność TUI: ramki paneli, wiersz zadania, globalny harness

## Summary

Lista i szczegóły dostają własne ramki, a ramka panelu z fokusem jest w kolorze akcentu i grubsza. Zadanie na liście zajmuje dwie linie: markery i tytuł, pod nimi wcięta etykieta. Globalny domyślny harness wraca do TUI: jest widoczny w nagłówku głównego okna i zmienia się klawiszem `h` z listy i ze szczegółów. W zadaniach w toku zmiana od razu obejmuje dyrygenta, a agentów od kolejnego kroku, chyba że workflow wskazuje dla kroku konkretny harness. Wybór harnessu dyrygenta per zadanie znika.

---

## Problem Frame

Po przeprojektowaniu TUI cały ekran ma jedną szarą ramkę. Lista ma tylko prawą krawędź, która przy fokusie robi się cyan. Szczegóły ramki nie mają w ogóle, a fokus zdradza im jedynie kolor zakładek. W praktyce to, który panel przyjmuje klawisze, widać głównie po kursorze.

Lista ma 28 kolumn. Wiersz `?● tytuł etykieta` skleja oba markery ze sobą, tytuł jest ucinany, a etykieta (krok, `#n`, `offline`) stoi zaraz za nim w ciemnoszarym. Trudno ją szybko przeczytać.

Domyślny harness demona to ustawienie globalne. Wybiera, na czym startują nowe zadania i dyrygenci. Stary TUI pozwalał go zmienić, nowy już nie. Został tylko wybór harnessu dyrygenta jednego zadania (`h` na zakładce *Dyrygent*). Operator nie widzi na głównym oknie, jaki harness obowiązuje, i nie może go zmienić bez CLI.

---

## Requirements

**Ramki i fokus**
- R1. Lista i szczegóły mają po jednej pełnej ramce. Jedna zewnętrzna ramka wokół całego ekranu znika, a nagłówek (`narsor · połączenie · status`, liczniki) zostaje w górnej linii ekranu.
- R2. Ramka panelu z fokusem jest w roli Focus (cyan) i grubsza (linia pogrubiona). Ramka panelu bez fokusu jest szara i cienka. Tytuł panelu ma kolor jego ramki.
- R3. Bez kolorów (`NO_COLOR`) aktywny panel nadal da się rozpoznać po grubości ramki i pogrubionym tytule.
- R4. Gdy szczegóły otwierają panel odpowiedzi i fokus jest w nim, ramka szczegółów pozostaje aktywna. Panel odpowiedzi zachowuje własną ramkę w roli Attention.
- R5. Na wąskim terminalu, gdzie widać jeden panel naraz, ten panel też ma ramkę w roli Focus.
- R6. Widoki w obszarze szczegółów (nowe zadanie, historia, repozytoria) i lista historii korzystają z tych samych ramek i reguły fokusu.

**Wiersz zadania**
- R7. Każde zadanie na liście zajmuje dwie linie. Pierwsza to marker uwagi, spacja, ikona stanu, dwie spacje i tytuł na całą pozostałą szerokość. Druga to etykieta wcięta pod początek tytułu.
- R8. Etykieta zachowuje dzisiejsze znaczenie i role kolorów (`offline` w Attention, `nie wysłano` w Problem, `#n` spokojnie, krok wyciszony). Podgląd przesuwania (`↕ n/m`) też pokazuje się w drugiej linii.
- R9. Zaznaczenie (reverse video) obejmuje obie linie zadania. Przewijanie i nawigacja przechodzą o całe zadanie, nigdy o pół.
- R10. Wiersze oczekujące (`↻ wysyłanie…`, `✗ nie wysłano`) i lista historii mają ten sam dwuliniowy układ.
- R11. Kolejność listy i zasada „stan to atrybut wiersza” się nie zmieniają.

**Globalny harness**
- R12. Nagłówek głównego okna zawsze pokazuje obowiązujący domyślny harness, np. `harness claude-code`. Gdy wartość pochodzi z konfiguracji (nie ma nadpisania), widać to, np. `harness claude-code (konfiguracja)`.
- R13. `h` otwiera wybór harnessu z listy i ze szczegółów, w każdej zakładce. Nie działa tylko wtedy, gdy fokus jest w polu tekstowym albo otwarta jest inna nakładka.
- R14. Wybór pokazuje harnessy obsługiwane przez demona i zaznacza obecny. Ma też pozycję „domyślny z konfiguracji (<nazwa>)”, która kasuje nadpisanie.
- R15. Po zmianie nagłówek pokazuje nową wartość, a powiadomienie ją potwierdza. Gdy zmiana się nie uda, powiadomienie o problemie podaje przyczynę, a nagłówek pokazuje starą wartość.
- R16. Zmiana obowiązuje od razu dla nowych zadań i dla dyrygentów wszystkich zadań, także tych w toku (od ich następnego wywołania).
- R16a. W zadaniu w toku krok agenta, który właśnie trwa, kończy się na swoim harnessie, łącznie z ponowieniami i wznowieniem sesji tego kroku. Dopiero kolejny krok agenta bierze nowy domyślny harness.
- R16b. Nowy domyślny harness nie dotyczy kroku, którego workflow wskazuje konkretny harness (`harness:` w akcji lub decyzji agenta). Nie dotyczy też zadania, któremu harness nadano jawnie przy tworzeniu (w żądaniu lub w profilu). Za domyślnym harnessem podążają tylko kroki, które same nie mają harnessu.
- R16c. Gdy backend zadania nie może uruchomić nowego harnessu (np. obraz go nie zawiera), kolejny krok zostaje na dotychczasowym harnessie zadania. Operator dostaje o tym jedną widoczną informację przy zadaniu i nie widzi żadnego błędu kroku.
- R17. Wybór harnessu dyrygenta per zadanie znika: klawisz `h` na zakładce *Dyrygent*, odpowiadająca mu komenda operatora w TUI i jej wpis w pomocy. Zakładka *Dyrygent* tylko pokazuje, na jakim harnessie dyrygent działa.
- R18. Pasek skrótów, pomoc (`?`) i README opisują `h` jako globalny wybór harnessu, a tabela TUI→CLI wskazuje jego odpowiednik w CLI.

---

## Acceptance Examples

- AE1. **Covers R2, R3.** Przy fokusie na liście ramka listy jest gruba i cyan, ramka szczegółów cienka i szara. Po `Tab` jest na odwrót. Z `NO_COLOR` różnica jest widoczna po grubości ramki.
- AE2. **Covers R7, R9.** Zadanie „Refactor parser” w kroku `implement` zajmuje dwie linie, a `j` przenosi zaznaczenie na całe następne zadanie, z obiema liniami w reverse video.
- AE3. **Covers R12, R14, R16.** Konfiguracja ma `claude-code`, a operator wybiera `opencode`. Nagłówek pokazuje `harness opencode`, a następne wywołanie dyrygenta zadania w toku idzie na opencode. Gdy operator wybierze potem „domyślny z konfiguracji”, nagłówek pokazuje `harness claude-code (konfiguracja)`.
- AE5. **Covers R16a, R16b.** Zadanie jest w kroku `implement` na claude-code, gdy operator wybiera opencode. `implement` kończy się na claude-code, także jeśli zostanie ponowiony. Następny krok `review` nie ma `harness:` w workflow, więc rusza na opencode. Krok `analyze` z `harness: claude-code` w workflow dalej idzie na claude-code.
- AE6. **Covers R16c.** Zadanie działa na zdalnym backendzie, którego obraz ma tylko claude-code, a operator wybiera opencode. Kolejny krok rusza na claude-code, a przy zadaniu widać informację, że nowy harness jest tu niedostępny.
- AE4. **Covers R13.** W polu tekstowym (czat dyrygenta, panel odpowiedzi, formularz) `h` wpisuje literę i nie otwiera wyboru.

---

## Success Criteria

- Operator na pierwszy rzut oka wie, który panel przyjmuje klawisze, i czyta tytuł oraz krok zadania bez wysiłku.
- Obowiązujący harness widać zawsze na głównym oknie, a zmiana nie wymaga wyjścia z TUI.
- Plan nie musi wymyślać zachowania: ramki, wiersz, klawisz i zasięg zmiany harnessu są tu określone.

---

## Scope Boundaries

- Nie przerywamy ani nie przełączamy kroku agenta, który właśnie trwa. Zmiana działa od następnego kroku.
- Nie dodajemy wyboru harnessu per zadanie przy tworzeniu (formularz nowego zadania dalej używa domyślnego).
- Bez zmian palety ról i zasady „tylko 16 kolorów ANSI”.
- Szerokość listy (28 kolumn) zostaje. Czytelność ma przyjść z układu dwuliniowego.

---

## Key Decisions

- Osobne ramki na panel zamiast kolorowanego separatora. Separator był za słabym sygnałem, a koszt to jedna kolumna.
- Dwie linie na zadanie zamiast szerszej listy. Tytuł mieści się w całości, za to na ekranie jest o połowę mniej zadań.
- Jedno ustawienie harnessu (globalne, obejmuje dyrygentów w toku), bez wyboru per zadanie. Jedno źródło prawdy zamiast dwóch poziomów nadpisań.
- W zadaniach w toku zmiana obejmuje dyrygenta od razu, a agentów od kolejnego kroku bez własnego harnessu. Operator chce, żeby przełączenie obejmowało też pracę, która już trwa, ale bez przerywania kroku w połowie. To zmienia dzisiejszą zasadę „harness przypięty per zadanie” (R42 w `src/environment/resolver.rs`) dla zadań, których harness pochodzi z wartości domyślnej.
- `h` jako klawisz globalny. Zwolnił się po usunięciu wyboru per zadanie, a `H` jest zajęte przez historię.

---

## Dependencies / Assumptions

- Globalny wybór istnieje po stronie demona (trwałe nadpisanie z wartością z konfiguracji jako fallbackiem, API odczytu, ustawienia i resetu). Dawny TUI z niego korzystał.
- Zakładamy, że TUI ma dostęp do tego wyboru także przy połączeniu z serwerem zdalnym (tryb sieciowy). To nie zostało sprawdzone.

---

## Outstanding Questions

### Deferred to Planning

- [Affects R16][Technical] Czy dyrygent zadania w toku czyta domyślny harness na żywo przy każdym wywołaniu, czy przypina go przy starcie wykonania (także na zdalnych backendach z kontrolerem)? Co zrobić z harnessem dyrygenta ustawionym wcześniej przez `SetConductorHarness`?
- [Affects R16a–R16c][Needs research] Jak odpiąć harness zadania (R42) dla kroków bez własnego harnessu. Jak runner na zdalnym backendzie dowiaduje się o zmianie (zdarzenie, komenda operatora, odczyt przy starcie kroku)? Gdzie zapisać, z jakiego harnessu wystartował który krok, żeby wznowienie i ponowienie zostały na tym samym? Jak przy tworzeniu zadania odróżnić harness jawny (żądanie, profil) od domyślnego?
- [Affects R16c][Technical] Skąd wiadomo, że backend lub obraz zadania nie ma danego harnessu (dziś sprawdza to `allows_harness` przy uruchomieniu) i jak pokazać tę informację przy zadaniu?
- [Affects R12–R15][Needs research] Przez który kanał TUI czyta i ustawia wybór w trybie sieciowym (API demona vs protokół control plane) i jak dostaje aktualizacje, gdy ktoś zmieni harness z CLI?
- [Affects R17][Technical] Czy komenda operatora `SetConductorHarness` i `narsor task conductor harness` w CLI też znikają, czy zostają tylko poza TUI?
- [Affects R1][Technical] Jak nagłówek z harnessem i licznikami mieści się w górnej linii przy minimalnej szerokości 60 kolumn (kolejność skracania)?
