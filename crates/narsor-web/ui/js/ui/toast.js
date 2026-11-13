import { html } from '../lib.js';

const ICONS = { success: ['✓', 'live'], warning: ['!', 'attention'], error: ['✗', 'problem'] };

/** The one notification slot; it lives 6 s (the store removes it). */
export function Toast({ notification }) {
  const [icon, role] = notification ? ICONS[notification.kind] ?? ICONS.warning : [];
  return html`
    <div class="toast-slot" aria-live="polite" role="status">
      ${notification && html`
        <div class=${`toast toast-${notification.kind}`} key=${notification.id}>
          <span class=${`role-${role}`} aria-hidden="true">${icon}</span> ${notification.text}
        </div>`}
    </div>`;
}
