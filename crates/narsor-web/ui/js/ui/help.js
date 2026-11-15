import { html, useEffect, useRef } from '../lib.js';

const KEYS = [
  ['j / ↓', 'następne zadanie'],
  ['k / ↑', 'poprzednie zadanie'],
  ['g / G', 'pierwsze / ostatnie'],
  ['Spacja', 'następne czekające na Ciebie'],
  ['/', 'filtr'],
  ['z', 'ukryj zakończone'],
  ['Enter / Esc', 'otwórz / zamknij szczegóły (wąski ekran)'],
  ['n', 'nowe zadanie'],
  ['.', 'akcje zadania'],
  ['[ / ]', 'poprzednia / następna zakładka'],
  ['K / J', 'wyżej / niżej w kolejce'],
  ['H', 'historia'],
  ['R', 'repozytoria'],
  ['h', 'domyślny harness'],
  ['L', 'limit zadań'],
  ['u', 'odśwież teraz'],
  ['?', 'ta pomoc'],
];

export function Help({ onClose }) {
  const ref = useRef(null);
  useEffect(() => { ref.current?.focus(); }, []);
  return html`
    <div class="overlay" onClick=${onClose}>
      <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="help-title" tabindex="-1" ref=${ref}
        onClick=${(event) => event.stopPropagation()}>
        <h2 id="help-title">Skróty klawiszowe</h2>
        <dl class="keys">
          ${KEYS.map(([key, text]) => html`<dt><kbd>${key}</kbd></dt><dd>${text}</dd>`)}
        </dl>
        <button class="primary" onClick=${onClose}>Zamknij</button>
      </div>
    </div>`;
}
