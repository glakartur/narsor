# Dokumentacja techniczna: GitHub Copilot SDK

**GitHub Copilot SDK** to zestaw oficjalnych bibliotek programistycznych umożliwiających osadzanie produkcyjnego środowiska wykonawczego agentów AI (tego samego, które napędza *GitHub Copilot CLI*) bezpośrednio we własnych aplikacjach, skryptach oraz systemach automatyzacji (np. w procesach CI/CD).

* Oficjalne repozytorium GitHub: [github/copilot-sdk](https://github.com/github/copilot-sdk)
* Dokumentacja oficjalna: [GitHub Docs: Copilot SDK](https://docs.github.com/en/copilot/how-tos/copilot-sdk)
* Szczegółowy przewodnik po funkcjach i architekturze: [Copilot SDK Docs (GitHub Repository)](https://github.com/github/copilot-sdk/blob/main/docs/README.md)

---

## 1. Architektura i działanie

SDK komunikuje się z bezpiecznym silnikiem (runtime) opartym o proces demona/CLI (w większości środowisk runtime CLI jest domyślnie dołączany i zarządzany automatycznie przez SDK w tle za pomocą protokołu JSON-RPC). Dzięki temu programista nie musi ręcznie implementować pętli agenta, planowania zadań, zarządzania stanem konwersacji czy parsowania wywołań narzędzi (tool calls).

### Wspierane języki programowania:
* **TypeScript / Node.js** (`@github/copilot-sdk`)
* **Python** (`github-copilot-sdk`)
* **Go** (`github.com/github/copilot-sdk/go`)
* **.NET** (`GitHub.Copilot.SDK`)
* **Java** oraz **Rust** (zależnie od stabilności wydań pakietów)

---

## 2. Instalacja i Wymagania wstępne

### Wymagania systemowe:
* Aktywny abonament GitHub Copilot (Individual, Business lub Enterprise) zapewniający uprawnienia do zapytań (Premium Requests).
* Odpowiednie wersje środowisk uruchomieniowych (np. Node.js w wersji 20.19+ / 22.12+, Python 3.11+, Go 1.24+).

### Szybka instalacja wybranych pakietów:

* **Node.js / TypeScript:**
  ```bash
  npm install @github/copilot-sdk
  ```
* **Python:**
  ```bash
  pip install github-copilot-sdk
  # Opcjonalne pobranie runtime wprzód:
  python -m copilot download-runtime
  ```

---

## 3. Szybki start (Quick Start)

### Przykład w Pythonie:
```python
import asyncio
from copilot import CopilotClient, PermissionHandler

async def main():
    async with CopilotClient() as client:
        async with await client.create_session(
            on_permission_request=PermissionHandler.approve_all,
            model="gpt-4.1",
        ) as session:
            response = await session.send_and_wait("Napisz prosty serwer w FastAPI")
            print(response.data.content)

asyncio.run(main())
```
*Źródło przykładu:* [Blog ExplainX o Copilot SDK](https://explainx.ai/blog/github-copilot-sdk-multi-platform-agents-python-typescript-go-dotnet-2026)

### Przykład w TypeScript / Node.js:
```typescript
import { CopilotClient, approveAll } from "@github/copilot-sdk";

const client = new CopilotClient();
const session = await client.createSession({
  onPermissionRequest: approveAll,
  model: "gpt-4.1",
});

const response = await session.sendAndWait({ prompt: "Wyjaśnij działanie hooków w React" });
console.log(response?.data.content);

await client.stop();
```

---

## 4. Uwierzytelnianie (Authentication)

SDK obsługuje kilka metod przekazywania poświadczeń (szczegóły w [dokumentacji uwierzytelniania](https://github.com/github/copilot-sdk/blob/main/docs/README.md)):
1. **Lokalny użytkownik GitHub (domyślnie):** Automatycznie pobiera token z zalogowanej sesji GitHub CLI (`gh auth login` lub `copilot login`).
2. **Zmienne środowiskowe:** Ustawienie tokenu w zmiennych `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` lub `GITHUB_TOKEN` (przydatne w środowiskach CI/CD, takich jak GitHub Actions).
3. **Bring Your Own Key (BYOK):** Możliwość podania własnych kluczy API (np. OpenAI, Azure, Anthropic), co pozwala na ominięcie limitów subskrypcji Copilota i pracę w sieciach odizolowanych.

---

## 5. Kluczowe funkcje i możliwości zaawansowane

Wszystkie zaawansowane mechanizmy zostały opisane w sekcji [Features w repozytorium SDK](https://github.com/github/copilot-sdk/blob/main/docs/features/README.md):

* **The Agent Loop:** Automatyczny cykl wykonywania zadań przez model, dobierania narzędzi, iteracji i obsługi błędów.
* **Hooks (Haczyki):** Przechwytywanie zdarzeń w cyklu życia sesji (np. modyfikacja zapytań użytkownika, kontrola nad zatwierdzaniem narzędzi przed ich wykonaniem `pre-tool-use`).
* **Skills (Umiejętności):** Ładowanie spakowanych folderów zawierających pliki `SKILL.md` z instrukcjami domenowymi dla agenta (zobacz [opis mechanizmu Skills](https://github.com/github/copilot-sdk/blob/main/docs/features/skills.md)).
* **MCP Servers (Model Context Protocol):** Integracja zewnętrznych serwerów MCP w celu rozszerzenia narzędzi dostępnych dla sesji Copilota.
* **Fleet Mode:** Uruchamianie równoległych sub-agentów do zadań podzielnych na niezależne podzadania.
* **Sesje chmurowe i zdalne:** Przenoszenie stanu sesji i ich synchronizacja z usługami GitHub.

### Skills w Narsor

W Narsor adapter GitHub Copilot SDK obsługuje skills na kilka sposobów:

1. **Poprzez `append_system_prompt` parametr** (dostępne teraz):
   ```yaml
   action:
     type: agent
     harness: github-copilot
     prompt: Analyze this code
     parameters:
       append_system_prompt: |
         # Code Review Skill
         Review for security and performance issues
   ```

2. **Poprzez katalog skills środowiska** (wymaga wrapper CLI):
   - Wrapper Copilot SDK może zaakceptować `--skills-dir` lub `--skill` flagi
   - SDK automatycznie ładuje i wstrzykuje skills do kontekstu sesji
   - Umożliwia na środowisko drive skill selection i reusable skill libraries

Szczegóły integracji skills z Narsor znajdują się w dokumentacji adaptera harnessu `github_copilot`.

---

## 6. Rozliczanie i Limiti (Billing)
Użycie SDK w ramach standardowego logowania konta GitHub Copilot rozlicza się zgodnie z polityką subskrypcji (każde zapytanie/prompt pomniejsza pulę zapytań/kredytów Premium Requests przypisanych do konta Individual, Business lub Enterprise). Szczegóły: [Dokumentacja rozliczeń GitHub Copilot](https://docs.github.com/copilot).