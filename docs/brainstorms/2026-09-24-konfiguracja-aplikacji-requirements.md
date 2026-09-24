---
date: 2026-09-24
topic: konfiguracja-aplikacji
---

# Konfiguracja aplikacji: ustawienia i ich pochodzenie

## Summary

Orchestrator dostaje warstwę ustawień aplikacji: plik konfiguracyjny rozwiązywany kaskadą,
zmienne środowiskowe i flagi ponad nim, oraz komendę, która wypisuje efektywną wartość każdego
ustawienia razem ze źródłem, z którego ta wartość przyszła. Konfiguracja jest do czytania,
nie do edycji z poziomu narzędzia.

---

## Problem Frame

Dziś orchestrator nie ma żadnej warstwy ustawień użytkownika. Jedyne pokrętła to flaga
`--root` i zmienna `RUST_LOG`; wybór harnessu agenta i limity czasu siedzą w definicjach
workflow, czyli w plikach opisujących *proces*, a nie *środowisko, w którym proces się wykonuje*.
Wszystko, co dotyczy maszyny — gdzie trzymać dane, którym harnessem mówić do agenta, jak długo
czekać — trzeba powtórzyć w wywołaniu albo w każdym workflow z osobna.

System dopiero powstaje i ten brak jest łapany wyprzedzająco, nie w reakcji na zaobserwowany ból
(patrz Dependencies / Assumptions). Znany jest natomiast kierunek, który zmienia kształt
rozwiązania: binarka ma docelowo trafić do kontenera na Kubernetesie, gdzie konfigurację
wstrzykuje się z zewnątrz, system plików bywa tylko do odczytu, a katalog domowy może nie
istnieć. Wtedy pytanie operatora nie brzmi „jak to zmienić", tylko „skąd wzięła się ta wartość
i dlaczego kontener zachowuje się inaczej niż laptop" — a na to dzisiaj nie da się odpowiedzieć
inaczej niż czytaniem kodu.

---

## Requirements

**Rozwiązywanie ustawień**

- R1. Każde ustawienie ma wartość wbudowaną, dzięki czemu orchestrator działa bez jakiejkolwiek
  konfiguracji zewnętrznej.
- R2. Ustawienia są rozwiązywane w jawnej, udokumentowanej kolejności pierwszeństwa: flaga
  wiersza poleceń, zmienna środowiskowa, plik konfiguracyjny, wartość wbudowana. Pierwsze
  trafienie wygrywa, per ustawienie — nie per plik.
- R3. Plik konfiguracyjny jest szukany kaskadą, pierwszy trafiony wygrywa i nie jest scalany
  z pozostałymi: jawnie wskazana ścieżka, katalog danych (`{root}`), katalog konfiguracyjny
  użytkownika. Kaskada jest tą samą ideą, którą orchestrator stosuje już przy rozwiązywaniu
  workflowów.
- R4. Brak pliku konfiguracyjnego w każdym z tych miejsc jest stanem poprawnym i cichym —
  nie ostrzeżeniem ani błędem.
- R5. Jeśli plik istnieje, ale jest niepoprawny — nieczytelny, źle sformułowany, z nieznanym
  kluczem albo wartością spoza dozwolonego zakresu — orchestrator odmawia startu z komunikatem
  wskazującym plik i miejsce błędu. Wartość wbudowana nie podmienia po cichu wartości, którą
  ktoś świadomie zapisał.
- R6. Każde ustawienie dostępne w pliku jest dostępne też jako zmienna środowiskowa, aby
  wstrzyknięcie konfiguracji do kontenera nie wymagało montowania woluminu.

**Podgląd**

- R7. Jest komenda wypisująca wszystkie ustawienia: nazwę, wartość efektywną i źródło tej
  wartości. Wypisuje komplet, także te pozostawione na wartości wbudowanej.
- R8. Źródło jest konkretne, nie kategoryczne: dla pliku podaje ścieżkę, która faktycznie
  została wczytana, dla zmiennej środowiskowej jej nazwę, dla flagi — flagę.
- R9. Podgląd działa bez zapisywalnego katalogu domowego i bez zapisywalnego systemu plików.
- R10. Podgląd ma postać nadającą się do wklejenia w zgłoszenie oraz postać nadającą się do
  przetworzenia maszynowo.
- R11. Wartości uznane za wrażliwe są w wypisie maskowane, a ich źródło pokazywane normalnie —
  „skąd to przyszło" jest ważniejsze i mniej niebezpieczne niż „co to jest".

**Zakres ustawień w v1**

- R12. Konfigurowalne są: katalog danych, domyślny harness agenta, limity czasu wykonania
  agenta oraz poziom logowania.
- R13. Ustawienie z pliku jest wartością domyślną, a nie przymusem: definicja workflow, która
  dziś jawnie określa harness albo limit czasu, nadal wygrywa nad konfiguracją.
- R14. Sekrety (tokeny, hasła, klucze) nie są przechowywane w tym pliku ani przez niego
  obsługiwane.

---

## Acceptance Examples

- AE1. **Dotyczy R2, R7, R8.** Dany plik konfiguracyjny ustawiający katalog danych i uruchomienie
  z flagą `--root` wskazującą gdzie indziej: wygrywa flaga, a podgląd pokazuje jako źródło
  właśnie flagę, nie plik.
- AE2. **Dotyczy R4, R7, R9.** Dany kontener bez pliku konfiguracyjnego i bez zapisywalnego
  katalogu domowego: orchestrator startuje normalnie, a podgląd wypisuje komplet ustawień
  ze źródłem „wartość wbudowana".
- AE3. **Dotyczy R5.** Dany zamontowany plik z nieznanym kluczem: każda komenda kończy się
  błędem wskazującym ten plik i ten klucz, zamiast startować na wartościach wbudowanych.
- AE4. **Dotyczy R13.** Dana konfiguracja wskazująca jeden harness i workflow, którego stan
  agenta jawnie wskazuje drugi: wykonanie używa harnessu z workflow.
- AE5. **Dotyczy R3, R8.** Dany plik konfiguracyjny obecny i w katalogu danych, i w katalogu
  użytkownika: wczytany zostaje tylko pierwszy z kaskady, a podgląd podaje jego ścieżkę —
  zawartość drugiego nie jest dołączana.

---

## Success Criteria

- Na pytanie „dlaczego ten kontener zachowuje się inaczej niż mój laptop" odpowiada jedno
  wywołanie, którego wynik da się wkleić do zgłoszenia — bez czytania kodu i bez zgadywania,
  która warstwa wygrała.
- Przeniesienie orchestratora na nową maszynę albo do kontenera nie wymaga edycji definicji
  workflow: to, co zależy od środowiska, daje się ustawić poza nimi.
- `ce-plan` nie musi wymyślać, które ustawienia istnieją, w jakiej kolejności są rozstrzygane
  ani co się dzieje przy braku i przy błędzie pliku — wszystkie cztery są tu rozstrzygnięte.

---

## Scope Boundaries

- Edycja konfiguracji z poziomu narzędzia — ani w CLI, ani w TUI. Plik edytuje się `$EDITOR`-em,
  a docelowo przyjeżdża z zewnątrz i jest tylko do odczytu.
- Rejestr repozytoriów, definicje workflow i kontekst egzekucji. To osobne konfiguracje z własnymi
  trybami podglądu; ta praca ich nie dotyka.
- Szerszy raport środowiska (rozwiązane workflowy, rejestr, obecność `git` i harnessów na `PATH`).
  Rozważony i odłożony: kusi, by urósł w health-check, a wartość „skąd ta wartość" dowozi już
  sam podgląd ustawień.
- Scalanie kilku plików konfiguracyjnych w jeden wynik. Pierwszy trafiony wygrywa w całości —
  scalanie jest dokładnie tym, co czyni pytanie „skąd ta wartość" trudnym.
- Konfiguracja per projekt czy per wykonanie. Ustawienia opisują środowisko, nie pojedynczy bieg.

---

## Key Decisions

- Plik z kaskadą, a nie same zmienne środowiskowe: nazwy zmiennych są trudniejsze do zmiany niż
  struktura pliku, a zestaw kluczy dopiero się ustala. Zmienne i tak istnieją (R6), tylko nie są
  jedynym mechanizmem.
- Podgląd zamiast edycji: w docelowym scenariuszu plik jest wstrzykiwany i niezapisywalny, więc
  edytor w narzędziu byłby ślepą uliczką, a potrzeba, którą trzeba pokryć, to pochodzenie wartości.
- Błędny plik zatrzymuje start, brak pliku nie: cicho zignorowana konfiguracja, którą ktoś
  świadomie zapisał, to najgorszy możliwy wynik w kontenerze.
- Konfiguracja ustępuje workflow (R13): definicja procesu jest normatywna, ustawienia opisują
  tylko środowisko.

---

## Dependencies / Assumptions

- **Brak dowodu z użycia.** Ta praca uzupełnia brak wyprzedzająco — nie ma zaobserwowanego
  przypadku, w którym czyjaś robota się o to rozbiła. Wynika stąd kryterium oceny: wartością
  jest to, że nie zabetonowano złych wartości domyślnych, a nie to, ile bólu usunięto.
  Jeśli po wdrożeniu któryś klucz nie jest nigdy ustawiany, kandyduje do usunięcia.
- Jedynym użytkownikiem jest dziś autor, ale kierunek na kontener w Kubernetesie jest założeniem
  wiążącym dla kształtu rozwiązania (R6, R9), a nie odległą możliwością.
- Zakłada się, że wrażliwe (R11) będą z czasem istnieć jakieś wartości; w v1 zbiór kluczy (R12)
  ich nie zawiera, więc maskowanie jest tanie do wprowadzenia, zanim stanie się potrzebne.

---

## Outstanding Questions

### Deferred to Planning

- [Dotyczy R12][Techniczne] Czy limit czasu agenta to jedno ustawienie, czy rozdziela się na
  osobne fazy wykonania — do rozstrzygnięcia przy oglądzie istniejącej obsługi akcji agenta.
- [Dotyczy R5][Techniczne] Czy odmowa startu przy błędnym pliku obowiązuje każdą komendę, czy
  podgląd konfiguracji jest wyjątkiem, który ma pokazać, co dokładnie jest zepsute.
- [Dotyczy R10][Techniczne] Format postaci maszynowej i jego relacja do obecnego podziału
  stdout/stderr w pozostałych komendach.
