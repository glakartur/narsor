// The sign-in form: the operator pastes a token. What token depends on how
// `narsor-web` runs (`GET /gateway/v1/info`):
//
// - `server`: narsor-web holds the server's token; the browser needs the
//   session token of narsor-web (`narsor-web --open` puts it in the address,
//   or it is in the file `web-session` of the state directory);
// - `passthrough`: the browser presents the server's own token (an OIDC
//   access token of the installation owner; no OIDC in the browser yet).

import { html, useEffect, useState } from '../lib.js';

const REASONS = {
  unauthorized: 'Token nie jest ważny albo wygasł. Wklej nowy token.',
  forbidden: 'To konto nie jest właścicielem instalacji. Użyj tokenu właściciela.',
};

function useMode() {
  const [mode, setMode] = useState(null);
  useEffect(() => {
    fetch(new URL('../gateway/v1/info', window.location.href), { cache: 'no-store', credentials: 'omit' })
      .then((response) => (response.ok ? response.json() : null))
      .then((info) => setMode(info?.token ?? null), () => setMode(null));
  }, []);
  return mode;
}

function Guide({ mode }) {
  if (mode === 'server') {
    return html`
      <details class="guide" open>
        <summary>Skąd wziąć token?</summary>
        <section>
          <h3>Token sesji narsor-web</h3>
          <ol>
            <li>Uruchom <code>narsor-web --open</code>. Program otworzy tę stronę z tokenem — nic nie trzeba wklejać.</li>
            <li>Albo skopiuj zawartość pliku <code>web-session</code> z katalogu stanu (domyślnie <code>~/.narsor/web-session</code>) i wklej ją powyżej.</li>
          </ol>
          <p class="hint">narsor-web tworzy nowy token sesji przy każdym starcie. Token serwera zostaje w narsor-web i nie trafia do przeglądarki.</p>
        </section>
      </details>`;
  }
  return html`
    <details class="guide" open=${mode === 'passthrough'}>
      <summary>Skąd wziąć token?</summary>
      <section>
        <h3>Token dostępu serwera</h3>
        <ol>
          <li>Wklej token dostępu (OIDC) właściciela instalacji, wydany przez dostawcę tożsamości Twojej organizacji. Token dostaniesz od administratora instalacji.</li>
          <li>Wygodniej: uruchom na swoim komputerze <code>narsor-web</code> z <code>web_upstream</code> ustawionym na adres serwera. Wtedy token zostaje w narsor-web, a przeglądarka dostaje tylko token sesji.</li>
        </ol>
        <p class="hint">Token dostępu wygasa. Wtedy ta strona wróci tutaj i poprosi o nowy token.</p>
      </section>
    </details>`;
}

export function SignIn({ reason, onToken }) {
  const [token, setToken] = useState('');
  const submit = (event) => {
    event.preventDefault();
    const value = token.trim().replace(/^Bearer\s+/i, '');
    if (value) onToken(value);
  };
  return html`
    <main class="signin">
      <form class="signin-card" onSubmit=${submit}>
        <img class="signin-logo" src="img/narsor-logo.svg" alt="Narsor" width="232" height="64" />
        ${reason && html`<p class="alert role-problem" role="alert">${REASONS[reason] ?? reason}</p>`}
        <label for="token">Token dostępu</label>
        <input id="token" type="password" autocomplete="off" spellcheck="false" autofocus
          placeholder="wklej token"
          value=${token} onInput=${(event) => setToken(event.currentTarget.value)} />
        <button type="submit" class="primary" disabled=${!token.trim()}>Połącz</button>
        <p class="hint">Token zostaje tylko w tej karcie przeglądarki. Zamknięcie karty go usuwa.</p>
        <${Guide} mode=${useMode()} />
      </form>
    </main>`;
}
