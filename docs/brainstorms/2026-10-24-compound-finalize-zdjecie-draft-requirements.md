---
date: 2026-10-24
topic: compound-finalize-zdjecie-draft
---

# Compound: niezawodne zdjęcie Draft w `finalize` i przegląd potrzeby `finalize_blocked`

## Summary

Krok `finalize` w procesie `compound-engineering` ma zdejmować Draft z pull requestu w sposób powtarzalny. Przyczyna obecnych porażek musi być najpierw ustalona na dowodach. Krok `finalize_blocked` ma być usunięty, jeśli samo `finalize` potrafi dokończyć pracę lub zakończyć proces z czytelnym powodem.

---

## Problem Frame

Artur zgłasza dwie sprawy:

1. `finalize` nie radzi sobie ze zdjęciem Draft. Artur podejrzewa, że kolejność operacji jest pomylona.
2. Krok `finalize_blocked` (bramka dla człowieka z wyborem `retry` / `finish_as_draft`) może być zbędny. Artur proponuje, aby proces nie wychodził z kroku `finish` (w pliku: `finalize`), dopóki nie wykona kompletu zadań tego kroku.

Stan obecny (sprawdzony w `workflows/compound-engineering.yaml`):

- `finalize` to krok agenta z `workspace_access: read-only` i `timeout: 10m`. Prompt ma trzy kroki: odczyt stanu, zdjęcie Draft, ponowny odczyt.
- Odpowiedź `blocked` i `timeout` prowadzą do `finalize_blocked`. Stamtąd `retry` wraca do `finalize`, a `finish_as_draft` kończy w stanie `left_as_draft` (status `cancelled`).
- Dwa przykłady poleceń w prompcie czytają stan inaczej: `gh` używa `isDraft`, a dla `glab mr view -F json` prompt nie podaje nazwy pola Draft.

Nie ustalono, dlaczego zdjęcie Draft zawodzi. To jest brak wiedzy, nie fakt (patrz „Założenia do potwierdzenia”).

---

## Requirements

**Przyczyna porażki zdjęcia Draft**

- R1. Przed zmianą ustal na dowodach (log kroku `finalize`, odpowiedź hosta), jaka operacja zawodzi. Zmiana kolejności jest jedną z hipotez, nie założeniem.
- R2. Zbadaj co najmniej te hipotezy: (a) `finalize` czyta stan zbyt wcześnie lub zbyt późno względem ostatniego pushu; (b) późniejsza operacja na PR przywraca Draft; (c) polecenie zdjęcia Draft lub odczyt stanu nie pasuje do hosta (GitHub, GitLab); (d) brak uprawnień lub logowania narzędzia `gh`/`glab`; (e) twarde ograniczenie `read-only` lub sieci blokuje wywołanie hosta.
- R3. Wynik badania zapisz w opisie zmiany: przyczyna, dowód, poprawka.

**Zachowanie `finalize`**

- R4. Gdy PR jest otwarty, `finalize` zdejmuje Draft, a potem odczytuje stan z hosta i potwierdza, że PR nie jest już Draft. Tylko to potwierdzenie daje odpowiedź `ready`.
- R5. Odczyt stanu działa jednakowo na GitHub i GitLab. Prompt podaje poprawne pole stanu dla każdego hosta.
- R6. Zachowane są obecne granice: brak zatwierdzenia, brak auto-merge, brak merge, brak czekania na checki, brak pushu, walidacja `pr_url`. PR już zmergowany lub już poza Draft daje `ready`.

**Przegląd kroku `finalize_blocked`**

- R7. Proces nie ma osobnego kroku `finalize_blocked`, jeśli każdy powód porażki ma jedną z dwóch dróg: (1) `finalize` sam naprawia i powtarza próbę w granicach limitu; (2) po wyczerpaniu prób proces kończy się czytelnym stanem końcowym z powodem.
- R8. Powtórzenia prób są ograniczone (liczba i czas). Proces nie czeka bez końca.
- R9. Powód, którego agent nie może naprawić (brak narzędzia, brak logowania, zły URL, zamknięty PR), jest widoczny dla człowieka bez bramki: w stanie końcowym i w komentarzu (patrz zadanie „komentarz w każdym stanie zatrzymania” z `docs/brainstorms/2026-10-22-akcje-taska-z-backendu-i-komentarz-w-pauzie-requirements.md`).
- R10. Jeśli bramka zostaje usunięta, usunięte są też: stan `left_as_draft` (lub jego rola jest przeniesiona), wpisy w testach (`tests/workflows.rs`, `tests/builtin_workflows.rs`), opis w `workflows/README.md` i diagram `workflows/compound-engineering-flow.html`.

---

## Acceptance Examples

- AE1. **Covers R4.** Given otwarty PR w Draft i działające `gh`, when `finalize` działa, then PR jest po kroku poza Draft, a odpowiedź `ready` zawiera stan odczytany z hosta.
- AE2. **Covers R4, R7.** Given zdjęcie Draft zwróciło sukces, ale ponowny odczyt pokazuje Draft, when `finalize` działa, then krok nie odpowiada `ready`; powtarza próbę albo kończy się z powodem.
- AE3. **Covers R5.** Given PR na GitLab, when `finalize` odczytuje stan, then używa pola Draft właściwego dla `glab` i nie wnioskuje stanu z tytułu bez potwierdzenia.
- AE4. **Covers R7, R9.** Given `glab` bez logowania, when `finalize` działa, then proces kończy się czytelnym powodem i nie czeka na człowieka w bramce.
- AE5. **Covers R6.** Given PR już zmergowany, when `finalize` działa, then odpowiedź to `ready` i brak zmian na hoście.

---

## Success Criteria

- PR kończy proces poza Draft w typowym przebiegu na GitHub i na GitLab, bez ręcznej interwencji.
- Gdy zdjęcie Draft jest niemożliwe, człowiek widzi powód od razu w stanie końcowym.
- Liczba stanów w procesie maleje (jeśli wybrano usunięcie bramki), a testy opisują nowy przepływ.
- `ce-plan` nie musi zgadywać, która hipoteza przyczyny jest prawdziwa: R1–R3 wymagają dowodu przed zmianą.

---

## Scope Boundaries

- Poza zakresem: zatwierdzanie, auto-merge i merge PR (zostają zadaniem człowieka).
- Poza zakresem: zmiana innych bramek (`review_*`, `check_*_feedback`).
- Poza zakresem: zmiana modelu uprawnień konta dla `gh`/`glab`.

---

## Key Decisions

- Przyczyna przed poprawką: zmiana kolejności operacji bez dowodu mogłaby nie usunąć problemu.
- Rekomendacja dla `finalize_blocked`: usunąć bramkę, jeśli powtórzenia w kroku plus czytelny stan końcowy pokrywają wszystkie powody. Człowiek i tak nie naprawi przyczyn środowiskowych w bramce w 10 minut; `retry` bez naprawy daje tę samą porażkę.

---

## Dependencies / Assumptions

- Mechanizm `retry_policy` istnieje w kroku `compound`. Nie sprawdzono, czy działa tak samo dla odpowiedzi `blocked` (a nie tylko dla błędu kroku).
- Zadanie „komentarz w każdym stanie zatrzymania” (#22) może dać drogę pokazania powodu bez bramki.

---

## Założenia do potwierdzenia

- Założenie 1: „krok `finish`” w pytaniu Artura oznacza krok `finalize` (w pliku nie ma kroku `finish`; jest stan końcowy `finished`).
- Założenie 2: „komplet zadań do wykonania” oznacza: PR poza Draft, potwierdzony odczytem z hosta.
- Założenie 3: Przyczyna porażki nie jest znana. Nie założono, że to kolejność operacji.
- Założenie 4: Dla powodów, których agent nie naprawi, proces ma się zakończyć, a nie czekać bez końca.
- Założenie 5: Stan końcowy po wyczerpaniu prób ma status inny niż `completed`, aby nie ukrywać porażki.

## Pytania otwarte dla użytkownika

1. Jak dokładnie `finalize` zawodzi? Prosimy o log lub komunikat (host: GitHub czy GitLab; błąd narzędzia, czy PR zostaje w Draft mimo sukcesu).
2. Czy w razie powodu, którego agent nie naprawi (brak logowania, zamknięty PR), proces ma: (a) zakończyć się porażką z powodem, czy (b) dalej czekać na człowieka w bramce?
3. Czy `left_as_draft` ma zostać jako stan końcowy, czy zastąpi go stan porażki?

---

## Outstanding Questions

### Resolve Before Planning

- [Affects R1] [User decision] Dostarczenie logu porażki `finalize` (pytanie 1).
- [Affects R7, R9] [User decision] Wybór drogi dla powodów nienaprawialnych (pytanie 2).

### Deferred to Planning

- [Affects R8] [Technical] Jak ograniczyć powtórzenia: `retry_policy` silnika czy pętla w prompcie.
- [Affects R2] [Needs research] Czy `read-only` workspace lub sandbox blokuje wywołania hosta.
- [Affects R5] [Needs research] Nazwa pola Draft w `glab mr view -F json`.
