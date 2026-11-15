// *Zdarzenia*: the workflow events of the task, read page by page
// (`after` / `has_more`). An `unavailable` answer is a note, not an error.

import { html } from '../../lib.js';
import { clock, eventDetail } from '../../text.js';
import { useRead } from '../read.js';

const PAGE_LIMIT = 500;
const MAX_PAGES = 20;
const cache = new Map();

async function readEvents(api, taskId, signal) {
  const known = cache.get(taskId) ?? { events: [], unavailable: null };
  let events = known.events;
  for (let page = 0; page < MAX_PAGES; page += 1) {
    const after = events.length ? events[events.length - 1].sequence : 0;
    const answer = await api.executionEvents(taskId, after, PAGE_LIMIT, signal);
    if (answer?.status !== 'available') {
      const result = { events, unavailable: answer?.message || answer?.reason || 'niedostępne' };
      cache.set(taskId, result);
      return result;
    }
    events = events.concat(answer.events ?? []);
    if (!answer.has_more || !answer.events?.length) break;
  }
  const result = { events, unavailable: null };
  cache.set(taskId, result);
  return result;
}

export function Events({ api, task }) {
  const { data, error, loading } = useRead((signal) => readEvents(api, task.task_id, signal), `${task.task_id}:${task.revision}`);
  return html`<div class="events">
    ${error && html`<p class="tab-note role-attention">${error}</p>`}
    ${data?.unavailable && !data.events.length && html`<p class="tab-note muted">zdarzenia niedostępne: ${data.unavailable}</p>`}
    ${data === null && loading && html`<p class="empty">wczytuję zdarzenia…</p>`}
    ${data?.events.length > 0 && html`
      <table class="event-table">
        <thead><tr><th>#</th><th>czas</th><th>zdarzenie</th></tr></thead>
        <tbody>
          ${data.events.map((event) => {
            const [type, ...rest] = eventDetail(event.data).split('  ');
            return html`<tr key=${event.sequence}>
              <td class="muted num">${event.sequence}</td>
              <td class="muted">${clock(event.timestamp)}</td>
              <td><code class="event-type">${type}</code> ${rest.join('  ')}</td>
            </tr>`;
          })}
        </tbody>
      </table>`}
    ${data && !data.unavailable && !data.events.length && html`<p class="empty">brak zdarzeń</p>`}
  </div>`;
}
