// The start of the application: take the token (from the address fragment
// or the session storage of the tab), remove it from the address bar, and
// draw the sign-in form or the application.

import { html, render } from './lib.js';
import { Api } from './api.js';
import { Store } from './store.js';
import { App } from './ui/app.js';
import { SignIn } from './ui/signin.js';

const TOKEN_KEY = 'narsor.token';

/** The address the API is under: the parent of `/ui/`, prefix kept. */
const apiBase = new URL('..', window.location.href).href;

function storage() {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

/** The token from `#token=…`, moved to the session storage; the fragment is removed. */
function takeToken() {
  const fragment = new URLSearchParams(window.location.hash.replace(/^#/, ''));
  const fromAddress = fragment.get('token');
  if (fromAddress) {
    storage()?.setItem(TOKEN_KEY, fromAddress);
    const url = new URL(window.location.href);
    url.hash = '';
    window.history.replaceState(null, '', url.pathname + url.search);
    return fromAddress;
  }
  return storage()?.getItem(TOKEN_KEY) ?? null;
}

const root = document.getElementById('app');
let store = null;

function signIn(token) {
  storage()?.setItem(TOKEN_KEY, token);
  start(token);
}

function signOut(reason = null) {
  store?.stop();
  store = null;
  storage()?.removeItem(TOKEN_KEY);
  showSignIn(reason);
}

function showSignIn(reason) {
  render(html`<${SignIn} reason=${reason} onToken=${signIn} />`, root);
}

function start(token) {
  store?.stop();
  const api = new Api(apiBase, token);
  store = new Store(api, { connectionName: window.location.host });
  const current = store;
  current.subscribe((state) => {
    if (state.auth && current === store) signOut(state.auth);
  });
  render(html`<${App} store=${current} onSignOut=${() => signOut()} />`, root);
  current.start();
}

const token = takeToken();
if (token) start(token);
else showSignIn(null);
