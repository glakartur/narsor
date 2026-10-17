# Nagrania Claude Code (`stream-json`)

Te pliki są źródłem prawdy o formacie `claude --print --output-format stream-json`
dla adaptera `claude-code` (plan `docs/plans/2026-10-16-2343-feat-zunifikowany-model-rozmowy-plan.md`, U1).
Plik `docs/sources/claude-code-json.md` nie jest źródłem prawdy.

- Wersja CLI: `claude` 2.1.302 (pole `claude_code_version` w `system/init`).
- Model: `sonnet` (`claude-sonnet-5-5`).
- Data nagrania: 2026-10-16.
- Katalog roboczy: pusty projekt testowy z plikiem `notes.txt` (`alpha`, `beta`, `gamma needle`)
  i skillem projektu `hello` (`.claude/skills/hello/SKILL.md`).

## Wspólne flagi

```text
claude --print --output-format stream-json --verbose --model sonnet --strict-mcp-config \
       [flagi scenariusza] --permission-mode <tryb> <prompt>
```

`--strict-mcp-config` wyłącza serwery MCP z konfiguracji użytkownika, aby nagrania nie zawierały
ich nazw. Narsor tej flagi nie przekazuje. Kształt linii jest taki sam.

Schemat dla scenariuszy `schema_*` (wersja płaska, patrz niżej):

```json
{"type":"object","properties":{"output":{"type":"string","enum":["done","blocked"]},"data":{"type":"object"},"variables":{"type":"object"},"artifacts":{"type":"array","items":{"type":"string"}},"status":{"type":"string","enum":["awaiting_input"]},"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}},"additionalProperties":false}
```

Prompty `schema_*` mają na końcu skrócony tekstowy kontrakt wyjścia (label `done` albo `blocked`,
albo `awaiting_input`).

## Scenariusze

| Plik | Flagi scenariusza | Co pokazuje |
|------|-------------------|-------------|
| `tools.ndjson` | `--permission-mode acceptEdits` | `Read`, `Bash`, `Grep` (przez `Bash`), `Skill`; po `Skill` linia `user` z blokiem `text` i `isSynthetic: true` (treść skilla) |
| `tool_error.ndjson` | `--permission-mode acceptEdits` | `system/permission_denied`, `tool_result` z `is_error: true` dla `Bash` i `Read` |
| `thinking.ndjson` | `--permission-mode acceptEdits` | blok `thinking`, `system/thinking_tokens` |
| `max_turns.ndjson` | `--max-turns 1 --permission-mode acceptEdits` | `result` z `subtype: error_max_turns`, `is_error: true`, `errors[]`, bez pola `result`; kod wyjścia 1 |
| `ask_user_question.ndjson` | `--permission-mode acceptEdits` | prośba o `AskUserQuestion`: agent woła `ToolSearch`, narzędzia nie ma, agent pyta tekstem |
| `plan_mode.ndjson` | `--permission-mode plan` | tryb `plan`: agent pisze plan przez `Write` do `~/.claude/plans/…`, nie zmienia projektu |
| `schema_done.ndjson` | `--json-schema <schemat> --permission-mode acceptEdits` | `structured_output` z `output: done` i `data` |
| `schema_awaiting.ndjson` | `--json-schema <schemat> --permission-mode acceptEdits` | `structured_output` z `status: awaiting_input`, `question`, `options` |
| `schema_resume.ndjson` | `--json-schema <schemat> --resume <sesja schema_done> --permission-mode acceptEdits` | schemat działa po `--resume`; hooki `SessionStart:resume` |
| `schema_plan_resume.ndjson` | `--json-schema <schemat> --resume <sesja schema_done> --permission-mode plan` | kształt repromptu narsora (tekst `OUTPUT_REPROMPT`) |
| `schema_plan_fresh.ndjson` | `--json-schema <schemat> --permission-mode plan` | kształt wywołania dyrygenta (nowa sesja, `plan`) |
| `schema_mismatch.ndjson` | `--json-schema <schemat> --permission-mode acceptEdits` | prompt żąda labela spoza `enum`; CLI odrzuca wywołanie `StructuredOutput` i model poprawia odpowiedź |

## Odpowiedzi na pytania odłożone do implementacji

1. **Miejsce `structured_output`.** Pole `structured_output` jest na linii `result` (obok `result`)
   w trybie `stream-json`. Działa także po `--resume` i w trybie `plan`. Pole `result` zawiera wtedy
   ten sam JSON jako tekst.
2. **Mechanizm.** CLI dodaje narzędzie `StructuredOutput`. Model woła je jako `tool_use`
   (`name: "StructuredOutput"`, `input` = obiekt wyniku), a CLI odpowiada `tool_result`
   `"Structured output provided successfully"`. Gdy model kończy turę bez tego narzędzia, CLI wysyła
   linię `user` z `isSynthetic: true` i tekstem `[structured-output-enforce] You MUST call the
   StructuredOutput tool…`.
3. **Niezgodność ze schematem.** CLI waliduje `input`. Przy niezgodności `tool_result` ma
   `is_error: true` i treść `Output does not match required schema: /output: must be equal to one of
   the allowed values: [...]`. Model próbuje ponownie (`schema_mismatch.ndjson`). Narsor musi nadal
   sprawdzić wynik sam (R11), bo nie wiemy, co CLI robi po wyczerpaniu prób.
4. **Słowa kluczowe schematu.** `anyOf`, `oneOf` i `allOf` na najwyższym poziomie są odrzucane przez
   API: `result` ma `is_error: true`, `subtype: success`, `terminal_reason: api_error`, tekst
   `API Error: 400 tools.9.custom.input_schema: input_schema does not support oneOf, allOf, or anyOf at
   the top level`, a proces kończy się kodem 1. Bez `"type":"object"` na najwyższym poziomie błąd to
   `input_schema.type: Field required`. Działają: `type: object`, `properties`, `enum`,
   `additionalProperties: false`, `items`. Dlatego narsor używa wersji zapasowej z planu: jeden obiekt
   z opcjonalnymi polami obu gałęzi (bez `required`) i walidacja po stronie narsora.
5. **`AskUserQuestion` w `--print`.** Narzędzie nie jest dostępne w trybie `--print` (2.1.302).
   Model szuka go przez `ToolSearch` (`select:AskUserQuestion`), dostaje `No matching deferred tools
   found` i pyta zwykłym tekstem. Element „pytanie agenta” ze źródłem „narzędzie” jest więc testowany
   na linii syntetycznej (kształt `tool_use` jak dla innych narzędzi, `input.questions[]`).
6. **Typy linii.** `system` z podtypami: `init`, `hook_started`, `hook_response`, `thinking_tokens`,
   `permission_denied`. Poza tym: `assistant`, `user`, `rate_limit_event`, `result`. W nagraniach nie
   ma kompaktowania (`compact_boundary`); jest testowane na linii syntetycznej.
7. **Myślenie.** Blok `thinking` ma pusty tekst (`"thinking": ""`) i podpis. Treść myślenia nie jest
   widoczna w strumieniu.
8. **`result` i metadane.** `total_cost_usd`, `usage`, `num_turns`, `duration_ms`, `stop_reason`,
   `terminal_reason`, `subtype`, `is_error`, `session_id`. Przy `error_max_turns` brak pola `result`,
   jest `errors[]`.

## Maskowanie

Skrypt maskowania zmienił tylko wartości, nie kształt linii:

- Ścieżka katalogu roboczego zamieniona na `/workspace/project`, katalog domowy na `/home/user`,
  imię użytkownika na `User`.
- Każdy UUID (sesje, `uuid`, `hook_id`) zamieniony na stały UUID testowy
  `00000000-0000-4000-8000-0000000000NN`. Ta sama wartość wejściowa daje ten sam UUID we wszystkich
  plikach, więc sesja `schema_done` i jej `--resume` mają ten sam identyfikator.
- Podpisy bloków `thinking` zamienione na `masked-signature`.
- W `system/init` listy `slash_commands`, `skills`, `agents`, `plugins`, `terminal_slash_commands`
  skrócone do kilku neutralnych wartości; `memory_paths` i `messaging_socket_path` zamaskowane.
- Treść `output`/`stdout`/`stderr` w liniach `hook_response` zamieniona na `<masked hook output>`.
- Identyfikatory `toolu_…` i `msg_…` zostały bez zmian (losowe, bez danych).

Pliki nie zawierają danych klientów ani sekretów. Przed ponownym nagraniem trzeba powtórzyć
maskowanie i sprawdzić pliki.
