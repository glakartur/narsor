---
date: 2026-10-24
topic: compound-finalize-zdjecie-draft
---

# Compound: niezawodne zdjęcie Draft w `finalize` i bramka `finalize_blocked`

## Summary

Krok `finalize` w procesie `compound-engineering` zdejmuje Draft z pull requestu i potwierdza to odczytem z hosta. Gdy `finalize` nie potrafi tego zrobić, proces nie przechodzi automatycznie do porażki. Zatrzymuje się na bramce `finalize_blocked`, gdzie człowiek wybiera: ponów próbę albo potwierdź porażkę. Bramki `finalize_blocked` nie ma dziś w pliku workflow, więc ta zmiana dodaje ją z powrotem. Stan `left_as_draft` jest zastąpiony stanem porażki `finalize_failed`.

---

## Problem Frame

Artur zgłosił dwie sprawy:

1. `finalize` nie radzi sobie ze zdjęciem Draft. Artur podejrzewa złą kolejność operacji.
2. Artur pytał, czy bramka `finalize_blocked` jest potrzebna.

W PR #25 (plan, linia 36) Artur rozstrzygnął drugą sprawę: przejście do `finalize_failed` nie może być automatyczne. Akcja wymaga zatwierdzenia przez człowieka. Dlatego ten dokument **nie usuwa** bramki. Poprzednia wersja dokumentu zalecała usunięcie bramki. Ta zalecenie jest cofnięte.

Stan obecny (sprawdzony w `workflows/compound-engineering.yaml`):

- `finalize` to krok agenta z `workspace_access: read-only` i `timeout: 10m`.
- Prompt ma dziś pętlę do 3 prób: odczyt, zdjęcie Draft, ponowny odczyt. Odczyt stanu jest już opisany dla GitHub (`isDraft`) i GitLab (`draft`, z rezerwą `work_in_progress` lub prefiks tytułu).
- Odpowiedzi `blocked` i `timeout` prowadzą dziś bezpośrednio do `finalize_failed` (stan końcowy, status `failed`). Bramki `finalize_blocked` w pliku już nie ma.
- Brak potwierdzonej przyczyny pierwotnej porażki. To brak wiedzy, nie fakt.

---

## Requirements

**Przyczyna porażki zdjęcia Draft**

- R1. Przed zmianą ustal na dowodach (log kroku `finalize`, odpowiedź hosta), jaka operacja zawodzi. Zmiana kolejności jest jedną z hipotez, nie założeniem.
- R2. Zbadaj co najmniej: (a) odczyt stanu zbyt wcześnie lub zbyt późno względem ostatniego pushu; (b) późniejsza operacja przywraca Draft; (c) polecenie lub pole stanu nie pasuje do hosta; (d) brak narzędzia lub logowania `gh`/`glab`; (e) ograniczenie `read-only` lub sieci blokuje host.
- R3. Wynik badania (przyczyna, dowód, poprawka) trafia do opisu zmiany.

**Zachowanie `finalize`**

- R4. Gdy PR jest otwarty i w Draft, `finalize` zdejmuje Draft, potem odczytuje stan z hosta. Tylko odczyt, który pokazuje PR poza Draft, daje `ready`.
- R5. PR już zmergowany daje `ready` po pierwszym odczycie. Ponowny odczyt po zdjęciu Draft dotyczy tylko PR otwartego, który był w Draft. PR zamknięty bez merge daje `blocked`.
- R6. Odczyt stanu działa jednakowo na GitHub i GitLab, z poprawnym polem Draft dla każdego hosta.
- R7. Obecne granice zostają: brak zatwierdzenia, brak auto-merge, brak merge, brak czekania na checki, brak pushu, walidacja `pr_url`.

**Bramka `finalize_blocked`**

- R8. Odpowiedź `blocked` i `timeout` kroku `finalize` prowadzą do bramki `finalize_blocked`, a nie wprost do stanu końcowego. Przejście do porażki wymaga decyzji człowieka.
- R9. Bramka ma dwa wybory: `retry` (powrót do `finalize`) i `confirm_failure` (przejście do `finalize_failed`). Zatwierdzenie samo w sobie nie jest ponowieniem: człowiek wybiera świadomie.
- R10. Opis bramki pokazuje powód z kroku `finalize` (z odpowiedzi `blocked`), aby człowiek mógł naprawić przyczynę przed `retry`. Powód nie zależy od zadania #22: wystarcza historia kroku i opis bramki.
- R11. Stan `left_as_draft` jest usunięty. Jego rolę przejmuje `finalize_failed` (status `failed`, nie `cancelled`), aby porażka była widoczna. Człowiek, który woli zostawić Draft, wybiera `confirm_failure`.
- R12. Liczba automatycznych prób w `finalize` jest ograniczona (obecnie 3). Pętla `retry` przez bramkę jest ręczna, więc nie powoduje nieskończonego czekania bez człowieka.
- R13. Testy (`tests/workflows.rs`, `tests/builtin_workflows.rs`), `workflows/README.md` i `workflows/compound-engineering-flow.html` opisują przepływ z bramką.

---

## Acceptance Examples

- AE1. **Covers R4.** Given otwarty PR w Draft i działające `gh`, when `finalize` działa, then PR jest poza Draft, a `ready` zawiera stan odczytany z hosta.
- AE2. **Covers R4, R8.** Given zdjęcie Draft zwróciło sukces, ale odczyt nadal pokazuje Draft po 3 próbach, when `finalize` kończy, then odpowiedź to `blocked`, a proces czeka na bramce `finalize_blocked`.
- AE3. **Covers R5.** Given PR zmergowany, when `finalize` działa, then odpowiedź to `ready` bez zmian na hoście i bez ponownego odczytu.
- AE4. **Covers R8, R9, R10.** Given `glab` bez logowania, when `finalize` odpowiada `blocked`, then bramka pokazuje powód. Gdy człowiek zaloguje `glab` i wybierze `retry`, `finalize` działa ponownie.
- AE5. **Covers R9, R11.** Given człowiek na bramce wybiera `confirm_failure`, then proces kończy w `finalize_failed` ze statusem `failed`.
- AE6. **Covers R6.** Given PR na GitLab, when `finalize` odczytuje stan, then używa pola Draft z JSON `glab`, a tytułu tylko jako rezerwy z podaniem tego w `reason`.

---

## Success Criteria

- PR kończy proces poza Draft w typowym przebiegu na GitHub i GitLab bez ręcznej interwencji.
- Porażka nigdy nie następuje bez decyzji człowieka. Człowiek widzi powód i może ponowić próbę.
- `ce-plan` nie musi zgadywać przyczyny: R1–R3 wymagają dowodu przed zmianą.

---

## Scope Boundaries

- Poza zakresem: zatwierdzanie, auto-merge i merge PR.
- Poza zakresem: zmiana innych bramek (`review_*`, `check_*_feedback`).
- Poza zakresem: model uprawnień konta dla `gh`/`glab`.
- Poza zakresem: zadanie #22 (komentarz w każdym stanie zatrzymania). Ten dokument go nie wymaga.

---

## Key Decisions

- Bramka zostaje (decyzja Artura, PR #25): przejście do porażki wymaga człowieka. To cofa wcześniejszą rekomendację usunięcia bramki.
- Bramka daje `retry` i `confirm_failure`, bo `retry` po naprawie środowiska (logowanie, narzędzie) ma sens.
- `left_as_draft` znika. Jeden stan porażki jest prostszy niż dwa, a status `failed` nie ukrywa problemu.
- Przyczyna przed poprawką: zmiana kolejności bez dowodu może nie usunąć problemu.

---

## Dependencies / Assumptions

- Silnik obsługuje krok `human` z własnymi wyborami (jak `review_*`). Zweryfikować w planie.
- Opis bramki może pokazać powód z poprzedniego kroku. Nie sprawdzono mechanizmu. Zweryfikować w planie.

---

## Założenia do potwierdzenia

- Założenie 1: „krok `finish`” w pytaniu Artura to krok `finalize`.
- Założenie 2: „komplet zadań” to: PR poza Draft, potwierdzony odczytem z hosta.
- Założenie 3: Przyczyna porażki nie jest znana. Nie założono, że to kolejność operacji.
- Założenie 4: Komentarz Artura w PR #25 oznacza: bramka zostaje, a człowiek zatwierdza porażkę.
- Założenie 5: `left_as_draft` jest zastąpiony przez `finalize_failed`. Najbezpieczniejsza opcja, bo nie ukrywa porażki.
- Założenie 6: Ręczne `retry` na bramce jest dozwolone bez limitu. Człowiek kontroluje pętlę.

## Pytania otwarte dla użytkownika

1. Jak dokładnie `finalize` zawodzi? Prosimy o log lub komunikat (GitHub czy GitLab; błąd narzędzia, czy PR zostaje w Draft mimo sukcesu).
2. Czy bramka ma mieć limit ręcznych `retry`, czy bez limitu?

---

## Outstanding Questions

### Resolve Before Planning

- [Affects R1] Log porażki `finalize` (pytanie 1). Plan może zacząć od badania z R1–R2, jeśli log nie przyjdzie.

### Deferred to Planning

- [Affects R10] [Technical] Jak przekazać powód z `finalize` do opisu bramki.
- [Affects R2] [Needs research] Czy `read-only` workspace lub sandbox blokuje wywołania hosta.
