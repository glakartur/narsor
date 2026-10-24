---
date: 2026-10-25
topic: tui-panel-rozmowy-trzy-formularze
---

# TUI: panel rozmowy z formularzami wybieranymi przez backend

## Summary

Panel na dole szczegółów taska (dziś „Czeka na Ciebie", `src/tui/task_app/waiting.rs`) pokazuje w danej chwili jeden formularz: wykonanie akcji workflow, odpowiedź na pytanie agenta, swobodną rozmowę z agentem, potwierdzenie wejścia w krok po osiągnięciu limitu albo nadanie uprawnień agentowi. `Esc` przełącza formularze. Backend podaje, które formularze są dostępne, z jakimi polami i wartościami, oraz który jest domyślny. TUI tylko to wyświetla. Przy okazji pliki panelu trafiają do katalogów: jeden katalog panelu i podkatalog na każdy formularz.

---

## Problem Frame

Dziś jeden panel odpowiedzi miesza różne formularze (akcje kroku, pytanie agenta, komentarz do agenta, uprawnienia, limit wejść). Logika w `waiting.rs`, `src/tui/input_form.rs` i `src/tui/chooser.rs` sama zgaduje z rodzaju oczekiwania (`InputKind`, `composer`), co operator może zrobić. Skutki:

- Operator widzi akcje i pole komentarza naraz. Nie wiadomo, co jest główne.
- Swobodna rozmowa z agentem jest tylko dodatkiem do innych formularzy (akcja 0 na liście). Nie ma jej wtedy, gdy panel nie jest otwarty z powodu oczekiwania.
- Frontend sam decyduje, kiedy formularz jest dostępny. Reguły są w kodzie TUI, a nie po stronie backendu.
- Kod formularzy leży w płaskich plikach `src/tui/*.rs` i w `task_app/`, więc trudno znaleźć części jednej funkcji.

---

## Actors

- A1. Operator: czyta panel, przełącza formularze, odpowiada agentowi, wykonuje akcje.
- A2. Agent: pracuje, zadaje pytania, prosi o uprawnienia, kończy pracę.
- A3. Backend (kontroler/serwer): oblicza listę dostępnych formularzy.

---

## Key Flows

- F1. Agent zadał pytanie
  - **Trigger:** Agent zadał pytanie i czeka na odpowiedź.
  - **Steps:** Panel pokazuje formularz odpowiedzi na pytanie (wybór jednej opcji + opcjonalny komentarz). Operator wybiera i wysyła. `Esc` przełącza na swobodną rozmowę.
  - **Covered by:** R2, R5, R9, R10

- F2. Agent skończył pracę
  - **Trigger:** Agent skończył krok albo krok nieagentowy zakończył przetwarzanie.
  - **Steps:** Panel pokazuje formularz akcji workflow. Dla kroku agentowego `Esc` przełącza na swobodną rozmowę.
  - **Covered by:** R2, R6, R7, R11

- F3. Agent pracuje
  - **Trigger:** Agent jest w trybie pracy.
  - **Steps:** Żaden formularz nie jest dostępny. Panel nie przyjmuje danych.
  - **Covered by:** R3, R8

---

## Requirements

**Kontrakt z backendem**
- R1. Backend zwraca razem z taskiem listę dostępnych formularzy panelu. Dla każdego: rodzaj (`akcja workflow`, `odpowiedź na pytanie agenta`, `swobodna rozmowa`, `potwierdzenie wejścia w krok po limicie`, `nadanie uprawnień agentowi`), pola z typami, opcjami, wymagalnością i opisami, wartości wstępne oraz znacznik domyślnego formularza.
- R2. Backend sam rozstrzyga dostępność i domyślny formularz według reguł R5–R9 i R21–R22. TUI nie wylicza dostępności z rodzaju oczekiwania ani ze stanu agenta.
- R3. Pusta lista formularzy oznacza, że panel nie przyjmuje danych (R8).
- R4. Wysłanie formularza używa istniejących ścieżek API (odpowiedź na oczekujące wejście, komentarz do agenta). Zmiana kontraktu dotyczy tylko opisu formularzy, nie ich wysyłania. Szczegóły w planie.

**Reguły dostępności (po stronie backendu)**
- R5. Swobodna rozmowa jest dostępna, gdy krok jest agentowy i agent nie pracuje: czeka na odpowiedź, komentarz lub uprawnienia, albo skończył pracę.
- R6. Odpowiedź na pytanie agenta jest dostępna, gdy krok jest agentowy, agent zadał pytanie i czeka na odpowiedź. Wtedy ten formularz jest domyślny. Zawiera wybór jednej z opcji agenta i opcjonalny komentarz.
- R7. Akcja workflow jest dostępna, gdy krok jest nieagentowy albo agentowy z agentem nie w trybie pracy. Gdy agent skończył pracę albo krok nieagentowy skończył przetwarzanie, ten formularz jest domyślny.
- R8. Gdy agent pracuje, a krok jest agentowy, nie ma żadnego formularza.
- R9. Jeśli kilka formularzy spełnia warunki domyślności, pierwszeństwo ma odpowiedź na pytanie agenta, potem akcja workflow, potem rozmowa. Akcja workflow ma pierwszeństwo tylko wtedy, gdy agent albo krok (skrypt) zakończył pracę. Gdy agent czeka na komentarz lub uprawnienia, akcja workflow jest dostępna (R7), ale nie jest domyślna: domyślna jest rozmowa (albo nadanie uprawnień, R21). Założenie Z2.

**Wyświetlanie i przełączanie**
- R10. Panel w danej chwili pokazuje jeden formularz, w tym samym miejscu co dziś.
- R11. `Esc` przełącza cyklicznie na następny dostępny formularz, zawsze (też gdy focus jest w polach). Przy jednym formularzu `Esc` nic nie zmienia. `Esc` nie zwija panelu i nie wraca do listy akcji.
- R12. Panel zawsze pokazuje, który formularz jest aktywny i ile ich jest (np. nazwa i `1/3`), oraz że `Esc` przełącza.
- R13. Zmiana listy formularzy (nowy odczyt) nie zmienia formularza, który operator ogląda, jeśli nadal jest dostępny i backend nie wskazał nowego domyślnego formularza (R14). Gdy zniknął, panel przechodzi na domyślny. Wpisany tekst nie ginie przy przełączaniu: każdy formularz pamięta swój szkic, jak dziś wpisy (`Entry`). Po ponownym przełączeniu na formularz wpisana treść wraca.
- R14. Gdy backend wskaże nowy domyślny formularz (np. agent zadał pytanie), panel przełącza się na niego, nawet jeśli operator oglądał inny. Szkic poprzedniego zostaje. Założenie Z3.
- R15. Klawisz `Tab` zachowuje obecną rolę: przenosi focus między panelem a resztą szczegółów. Panel nie zwija się, dopóki jest w nim dostępny jakikolwiek formularz, a w szczególności swobodna rozmowa. Założenie Z1.

**Formularz swobodnej rozmowy**
- R16. Pole tekstowe wielolinijkowe. Zaczyna od jednej linii i rośnie automatycznie z wpisanym tekstem (do rozsądnego limitu wysokości z przewijaniem; limit ustala plan).
- R17. Obowiązują reguły edycji z `2026-10-22-tui-edycja-formularzy-requirements.md` (kursor, zawijanie, `Ctrl+J` dla nowej linii, potwierdzenie przed wysłaniem).

**Dodatkowe formularze**
- R21. Nadanie uprawnień agentowi jest osobnym rodzajem formularza. Jest dostępne, gdy agent czeka na uprawnienia, i wtedy jest domyślne (jak odpowiedź na pytanie, R9). Założenie Z5.
- R22. Potwierdzenie wejścia w krok workflow po osiągnięciu limitu jest osobnym rodzajem formularza. Jest dostępne, gdy limit wejść w krok jest osiągnięty, i wtedy jest domyślne. Założenie Z5.

**Organizacja kodu**
- R18. Pliki panelu trafiają do jednego katalogu panelu rozmowy w szczegółach taska. Każdy formularz (z R1) ma własny podkatalog (stan, widok, klawisze, testy tego formularza). Wspólny kod panelu (przełączanie, nagłówek, kolejka szkiców) leży w katalogu panelu.
- R19. Przeniesienie plików nie zmienia zachowania poza opisanym tu. Testy istniejących funkcji przechodzą po przeniesieniu.
- R20. Frontend ma tylko logikę wyświetlania i obsługi klawiszy. Reguły R5–R9 są tylko w backendzie i mają testy w backendzie.

---

## Acceptance Examples

- AE1. Agent zadał pytanie: panel pokazuje formularz odpowiedzi (R6). `Esc` pokazuje swobodną rozmowę, a drugi `Esc` wraca do odpowiedzi.
- AE2. Agent pracuje: panel nie ma formularza (R8). Gdy agent skończy, pojawia się formularz akcji workflow.
- AE3. Krok nieagentowy skończył przetwarzanie: tylko formularz akcji, `Esc` nic nie zmienia (R7, R11).
- AE4. Operator pisze rozmowę, w tle przychodzi pytanie agenta: panel przechodzi na odpowiedź (R14), a po `Esc` szkic rozmowy jest nadal w polu (R13).

---

## Scope Boundaries

- Nie zmieniamy formatu rozmowy w zakładkach ani osi czasu.
- Nie zmieniamy ścieżek wysyłania odpowiedzi, komentarza i akcji (R4), ani reguł walidacji po stronie serwera.
- Panele operatora (propozycje dyrygenta, błąd kroku, komendy właściciela) zostają poza zakresem, o ile plan nie wykaże, że muszą być tym samym modelem.

---

## Założenia do potwierdzenia

- Z1. Potwierdzone przez recenzenta. Komentarz o `Tab` rozumiemy tak: `Tab` nadal opuszcza focus panelu, ale panel nie znika, gdy rozmowa jest możliwa. Dziś `Esc` na liście akcji zwija panel; po zmianie nic go nie zwija w tym przypadku. Potrzeba weryfikacji, czy panel bez żadnego formularza ma się ukrywać jak dziś.
- Z2. Kolejność pierwszeństwa domyślnego formularza z R9, z zastrzeżeniem, że akcja workflow jest domyślna tylko po zakończeniu pracy agenta lub kroku.
- Z3. Nowy domyślny formularz od backendu przejmuje widok (R14), nawet gdy operator pisze w innym. Treść wpisana przez operatora nie ginie i wraca po ponownym przełączeniu (R13).
- Z4. Uprawnienia i limit wejść to osobne rodzaje formularzy (R21, R22), nie część „akcji workflow". Problem wyniku (dziś osobny rodzaj `InputKind`) mieści się w „akcji workflow" albo „odpowiedzi na pytanie". Plan ustali dokładne mapowanie.
- Z5. Formularze z R21 i R22 są domyślne, gdy mają zastosowanie, przed odpowiedzią na pytanie i akcją workflow. Kolejność między nimi ustala plan.

## Pytania otwarte dla użytkownika

- Jak nazwać formularze w nagłówku panelu (po polsku)?
- Czy limit wzrostu pola rozmowy ma być stały (np. 8 linii), czy zależny od wysokości terminala?
