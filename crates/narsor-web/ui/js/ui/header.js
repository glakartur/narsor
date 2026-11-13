import { html } from '../lib.js';
import { counters, slots } from '../model.js';
import { connectionText, staleLabel } from '../store.js';
import { harnessText, usageGroups } from '../text.js';

export function Header({ state, now, onHelp, onSignOut, onDialog }) {
  const { connection, tasks } = state;
  const conn = connectionText(connection, now);
  const age = staleLabel(connection, now);
  const list = tasks ?? [];
  const selected = list.find((task) => task.task_id === state.selectedTaskId);
  const slot = slots(list, selected);
  const { questions, problems } = counters(list);
  const usage = usageGroups(state.usage, now);
  return html`
    <header class="topbar">
      <div class="topbar-left">
        <img class="brand-icon" src="img/narsor-icon.svg" alt="" width="24" height="24" /><span class="brand">narsor</span>
        <span class="muted conn-name">${connection.name}</span>
        <span class=${`conn role-${conn.role}`} role="status" aria-live="polite">
          <span aria-hidden="true">${conn.icon}</span> ${conn.text}
        </span>
        ${age && html`<span class="muted">· ${age}</span>`}
      </div>
      <div class="topbar-right">
        <button class="chip" title="Domyślny harness (h)" onClick=${() => onDialog('harness')}>${harnessText(state.harness, state.harnessPending)}</button>
        ${slot && html`<button class="chip" title="Limit zadań (L)" onClick=${() => onDialog('limit')}>sloty ${slot.used}${slot.limit !== null ? `/${slot.limit}` : ''}</button>`}
        <span class=${`chip counter ${questions ? 'role-attention' : 'role-done'}`} title="Czekają na odpowiedź">? ${questions}</span>
        <span class=${`chip counter ${problems ? 'role-problem' : 'role-done'}`} title="Mają problem">! ${problems}</span>
        <button class="small" title="Historia (H)" onClick=${() => onDialog('history')}>Historia</button>
        <button class="small" title="Repozytoria (R)" onClick=${() => onDialog('repositories')}>Repozytoria</button>
        <button class="icon" title="Skróty klawiszowe (?)" aria-label="Skróty klawiszowe" onClick=${onHelp}>⌨</button>
        <button class="icon" title="Wyloguj (usuń token z karty)" aria-label="Wyloguj" onClick=${onSignOut}>⏻</button>
      </div>
      ${usage.length > 0 && html`
        <div class="usage" aria-label="Limity użycia">
          ${usage.map((group) => html`
            <span class="usage-group">
              <strong>${group.name}</strong>
              ${group.limits.map((limit) => html`<span class=${limit.role ? `role-${limit.role}` : ''}>${limit.role ? '! ' : ''}${limit.text}</span>`)}
            </span>`)}
        </div>`}
    </header>`;
}
