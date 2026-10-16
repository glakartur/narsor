> **Uwaga:** ten dokument nie jest źródłem prawdy o formacie Claude Code. Źródło prawdy to nagrania prawdziwego `claude` w `tests/fixtures/claude-code/` i ich opis w `tests/fixtures/claude-code/README.md` (wersja CLI, scenariusze, odpowiedzi na pytania o `--json-schema`).

# Dokumentacja Techniczna API Strumieniowego i JSON w Claude Code

Niniejszy dokument opisuje strukturę komunikatów JSON zwracanych przez narzędzie **Claude Code** (autorstwa Anthropic) podczas pracy w trybie nieinteraktywnym (`-p` / `--print`). Dokument jest przeznaczony dla agentów programowych oraz skryptów automatyzacji parsujących wyjście CLI.

---

## 1. Przegląd flag wyjściowych

Claude Code w trybie nieinteraktywnym obsługuje trzy główne formaty wyjściowe poprzez flagę `--output-format`:
* **`text`** (domyślny) – czysty tekst czytelny dla człowieka.
* **`json`** – pojedynczy obiekt JSON zwracany na koniec zadania (z opcjonalnym wsparciem dla schematu).
* **`stream-json`** – strumień zdarzeń w formacie **NDJSON** (New-line Delimited JSON), wysyłany linia po linii w czasie rzeczywistym.

---

## 2. Format: `--output-format json` (Pojedynczy wynik)

Zwraca zwarty obiekt JSON podsumowujący wykonanie zadania.

### Schemat strukturalny
```json
{
  "type": "result",
  "result": "Wynik działania agenta...",
  "cost_usd": 0.0034,
  "session_id": "abc12345-6789-abcd-ef01-23456789abcd",
  "is_error": false
}
```

### Opis pól:
* **`type`** (`string`): Stała wartość `"result"`.
* **`result`** (`string`): Końcowa odpowiedź tekstowa wygenerowana przez model.
* **`cost_usd`** (`float`): Szacowany koszt zapytania/sesji w USD (przydatny do audytów w pipeline'ach CI/CD).
* **`session_id`** (`string`): Unikalny identyfikator UUID sesji. Umożliwia kontynuowanie konwersji w kolejnym wywołaniu za pomocą flagi `--resume <session_id>`.
* **`is_error`** (`boolean`): Flaga informująca, czy podczas wykonywania polecenia wystąpił błąd krytyczny (`true`/`false`).

---

## 3. Format: `--output-format stream-json` (Strumień NDJSON)

Każda linia wyjścia stanowi niezależny, poprawny obiekt JSON. Parser powinien czytać strumień linia po linijce (`readline`) i dekodować każdą linię osobno.

### Przykładowy ciąg zdarzeń w strumieniu:
```json
{"type": "system", "subtype": "init", "session_id": "abc12345...", "cwd": "/app"}
{"type": "progress", "message": "Analyzing repository structure..."}
{"type": "tool_use", "tool": "grep", "parameters": {"pattern": "TODO"}}
{"type": "result", "result": "Zadanie ukończone.", "cost_usd": 0.0041, "is_error": false}
```

### Szczegółowa specyfikacja typów zdarzeń w strumieniu (`type` / `subtype`):

1. **`type: "system"`** (Wiadomości inicjalizacyjne i kontrolne)
   * **`subtype: "init"`**: Zdarzenie początkowe emitowane w pierwszej linii strumienia.
     * Pola: `session_id`, `cwd` (katalog roboczy), `version` (wersja Claude Code).
2. **`type: "progress"`** (Status bieżący)
   * Informuje o postępach agenta w tle (np. skanowanie kodu, wstępna analiza).
   * Pola: `message` (`string` – czytelny komunikat o stanie).
3. **`type: "tool_use"`** (Użycie narzędzi wewnętrznych)
   * Emitowane, gdy Claude decyduje się użyć wbudowanego narzędzia (np. `grep`, `View`, `Bash`, `AskFollowupQuestion`).
   * Pola: `tool` (`string` – nazwa narzędzia), `parameters` (`object` – parametry przekazane do narzędzia).
4. **`type: "assistant"`** (Bloki odpowiedzi lub myśli modelu)
   * Reprezentuje cząstkowe lub pełne wygenerowane bloki tekstowe asystenta.
5. **`type: "result"`** (Zdarzenie końcowe)
   * Zawsze występuje jako **ostatnia linia strumienia**. Zawiera ostateczny wynik oraz statystyki kosztów i błędu (analogicznie jak tryb `--output-format json`).

---

## 4. Wymuszenie struktury: `--json-schema`

Tryb JSON pozwala na ścisłe zdefiniowanie oczekiwanego formatu odpowiedzi modelu za pomocą standardu **JSON Schema**.

### Przykład użycia w skrypcie:
```bash
SCHEMA='{"type": "object", "properties": {"status": {"type": "string"}, "errors": {"type": "array"}}, "required": ["status"], "additionalProperties": false}'

claude -p "Przeanalizuj logi" --output-format json --json-schema "$SCHEMA"
```

### Wynik ze schematem:
Gdy użyto `--json-schema`, odpowiedź zwrotna zawiera dodatkowe pole `structured_output` zawierające sparsowane i zweryfikowane dane zgodne z dostarczonym schematem:
```json
{
  "type": "result",
  "result": "...",
  "structured_output": {
    "status": "failed",
    "errors": ["NullPointerException in line 42"]
  },
  "cost_usd": 0.002,
  "is_error": false
}
```
*Wskazówka:* W skryptach automatyzacji wynik ten można bezpośrednio wyciągnąć za pomocą narzędzia `jq`: `| jq '.structured_output'`

---

## 5. Rekomendacje implementacyjne dla agenta

1. **Ignorowanie nieznanych typów:** Parser powinien odpornie traktować nowe, nieznane wartości pola `type` lub `subtype`, które mogą pojawić się w nowszych wersjach CLI.
2. **Obsługa interaktywności:** W trybie nieinteraktywnym zapytania o dane od użytkownika (`tool: "AskFollowupQuestion"`) oznaczają konieczność przerwaniaautomatyzacji, odczytania pytania z `parameters.question` i wznowienia sesji z użyciem flagi `--resume <session_id>`.
3. **Weryfikacja błędów:** Zawsze sprawdzaj pole `is_error` w zdarzeniu typu `result` przed uznaniem przebiegu za sukces.

---

## 6. Powiązane źródła i dokumentacja zewnętrzna

* [Claude Code Headless Mode & Output Formats (Zenn / Sora Biz)](https://zenn.dev/sora_biz/articles/claude-code-headless-mode?locale=en)
* [What is --output-format in Claude Code (ClaudeLog)](https://www.claudelog.com/faqs/what-is-output-format-in-claude-code/)
* [Structured CLI Output as Pipeline Glue (Steve Kinney)](https://stevekinney.com/courses/self-testing-ai-agents/)
* [LiteLLM Claude Code Harness Documentation](https://docs.litellm.ai/docs/harness/claude_code)