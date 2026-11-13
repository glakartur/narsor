// The sign-in form: the operator pastes a token. In trusted mode
// `narsor-cli web` opens the application with the token already in the
// address; in network mode the operator pastes an access token of the
// installation owner (no OIDC in the browser yet). The form tells where
// the token comes from in both modes.

import { html, useState } from '../lib.js';

const REASONS = {
  unauthorized: 'Token nie jest ważny albo wygasł. Wklej nowy token.',
  forbidden: 'To konto nie jest właścicielem instalacji. Użyj tokenu właściciela.',
};

function Guide() {
  return html`
    <details class="guide">
      <summary>Skąd wziąć token?</summary>
      <section>
        <h3>Serwer na tym komputerze</h3>
        <ol>
          <li>Uruchom w terminalu <code>narsor-cli web</code>. Komenda otworzy tę stronę z tokenem — nic nie trzeba wklejać.</li>
          <li>Albo skopiuj wartość pola <code>token</code> z pliku <code>~/.narsor/daemon.json</code> i wklej ją powyżej.</li>
        </ol>
        <p class="hint">Serwer tworzy nowy token przy każdym starcie. Po restarcie serwera zrób to jeszcze raz.</p>
      </section>
      <section>
        <h3>Serwer zdalny</h3>
        <ol>
          <li>Gdy masz skonfigurowane CLI, uruchom <code>narsor-cli --server ${'<adres>'} web</code>.</li>
          <li>Bez CLI: wklej token dostępu (OIDC) właściciela instalacji, wydany przez dostawcę tożsamości Twojej organizacji. Token dostaniesz od administratora instalacji.</li>
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
        <${Guide} />
      </form>
    </main>`;
}
