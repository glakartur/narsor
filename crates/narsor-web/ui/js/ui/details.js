// The details of the selected task: the header (workspaces, status row,
// usage row, notice) and the five tabs, in the TUI's order. The answer
// panel and the action panel come in U7–U8.

import { html } from '../lib.js';
import {
  branchText, notice, pullRequest, queuePositions, queueText, readiness, secondsBetween, shortAge,
  shortIdentifier, stateIcon, statusWord, stepIcon, titleOf,
} from '../model.js';
import { sumUsage, usageLine } from '../text.js';
import { AnswerPanel } from './answer.js';
import { useRead } from './read.js';
import { Conductor } from './tabs/conductor.js';
import { Conversation } from './tabs/conversation.js';
import { Events } from './tabs/events.js';
import { Info } from './tabs/info.js';
import { Progress } from './tabs/progress.js';

export const TABS = [
  ['conversation', 'Rozmowa'],
  ['progress', 'Przebieg'],
  ['events', 'Zdarzenia'],
  ['conductor', 'Dyrygent'],
  ['info', 'Info'],
];

const TERMINAL = new Set(['completed', 'failed', 'cancelled']);

/** The parts of the status row after the word, sorted by role as in the TUI. */
function statusParts(task, places, timeline) {
  const parts = [];
  const workflow = { text: `${task.workflow_id} v${task.workflow_version}`, role: 'done' };
  if (task.state === 'queued') return [{ text: queueText(task, places.get(task.task_id)), role: 'calm' }, workflow];
  const cursor = task.runner?.execution_cursor;
  const count = timeline?.positions?.length;
  const name = cursor?.current_state ? `${stepIcon(cursor.current_action_type, cursor.current_decision_by)} ${cursor.current_state}`.trim() : '';
  if (count || name) parts.push({ text: ['krok', count ? String(count) : null, count && name ? '·' : null, name || null].filter(Boolean).join(' '), role: 'calm' });
  parts.push(workflow);
  const until = TERMINAL.has(task.runner?.execution_status) ? task.runner.observed_at : task.as_of;
  parts.push({ text: shortAge(secondsBetween(task.created_at, until)), role: 'done' });
  if (task.state === 'running' && task.controller?.freshness !== 'live') {
    let text = 'brak kontrolera';
    if (task.controller) {
      text = task.controller.last_heartbeat_at
        ? `kontroler offline ${shortAge(secondsBetween(task.controller.last_heartbeat_at, task.as_of))}`
        : 'kontroler offline';
    }
    parts.push({ text, role: 'attention' });
  }
  const order = { attention: 0, calm: 1, done: 2 };
  return parts.sort((a, b) => order[a.role] - order[b.role]);
}

function Workspaces({ task }) {
  return (task.workspaces ?? []).map((workspace) => {
    const pr = pullRequest(workspace);
    const branches = branchText(workspace);
    return html`<div class="ws-row">
      <span class="muted">${workspace.name}</span>
      <strong>${workspace.repo}</strong>
      ${branches && html`<code>${branches}</code>`}
      ${(workspace.added !== undefined || workspace.removed !== undefined) && html`<span><span class="role-live">+${workspace.added ?? 0}</span> <span class="role-problem">−${workspace.removed ?? 0}</span></span>`}
      ${pr && (pr.url
        ? html`<a href=${pr.url} target="_blank" rel="noopener noreferrer">${pr.label} ↗</a>`
        : html`<span>${pr.label}</span>`)}
    </div>`;
  });
}

function Unstarted({ task, places }) {
  if (readiness(task) === 'archived') return html`<p class="empty">storage usunięty przy archiwizacji</p>`;
  const place = places.get(task.task_id);
  return html`<p class="empty">jeszcze nie wystartowało, ${place ? `#${place} ` : ''}w kolejce</p>`;
}

export function Details(props) {
  const { state, task } = props;
  if (!task) {
    return html`<section class="pane details-pane empty-details"><p class="empty">${state.tasks?.length ? 'wybierz zadanie' : 'brak zadań'}</p></section>`;
  }
  return html`<${DetailsBody} key=${task.task_id} ...${props} />`;
}

function DetailsBody({ store, state, task, now, onBack, onActions }) {
  const api = store.api;
  const icon = stateIcon(task);
  const note = notice(task);
  const places = queuePositions(state.tasks ?? []);
  const started = readiness(task) === 'started';
  const timelineRead = useRead(
    (signal) => (started ? api.timeline(task.task_id, signal) : Promise.resolve(null)),
    `${task.task_id}:${task.revision}:${started}`,
  );
  const timeline = timelineRead.data?.task_id === task.task_id ? timelineRead.data.timeline : null;
  const usage = usageLine(sumUsage(timeline?.positions));
  const tab = state.tab;

  let content;
  if (tab !== 'info' && !started) content = html`<${Unstarted} task=${task} places=${places} />`;
  else if (tab === 'conversation') content = html`<${Conversation} api=${api} task=${task} positions=${timeline?.positions ?? []} emptyText=${timelineRead.loading ? 'wczytuję…' : 'brak zapisanej rozmowy'} />`;
  else if (tab === 'progress') content = html`<${Progress} api=${api} task=${task} timeline=${timeline} now=${now} />`;
  else if (tab === 'events') content = html`<${Events} api=${api} task=${task} />`;
  else if (tab === 'conductor') content = html`<${Conductor} store=${store} task=${task} />`;
  else content = html`<${Info} task=${task} />`;

  return html`
    <section class="pane details-pane" aria-label="Szczegóły zadania">
      <div class="details-head">
        <div class="pane-head">
          <button class="small back" onClick=${onBack} aria-label="Wróć do listy">←</button>
          <h2 class="details-title" title=${task.task_id}>
            <span class="muted mono">${shortIdentifier(task)}</span> ${titleOf(task)}
          </h2>
          <button class="small" onClick=${onActions} title="Akcje (.)">Akcje</button>
        </div>
        <div class="details-meta">
          <${Workspaces} task=${task} />
          <div class="status-row">
            <span class=${`role-${icon.role}`}><span aria-hidden="true">${icon.glyph}</span> ${statusWord(task)}</span>
            ${statusParts(task, places, timeline).map((part) => html`<span class="muted"> · </span><span class=${`role-${part.role}`}>${part.text}</span>`)}
          </div>
          ${usage && html`<div class="usage-row muted mono">${usage}</div>`}
          ${note && html`<p class=${`notice role-${note.role}`}>${note.text}</p>`}
          ${timelineRead.error && html`<p class="tab-note role-attention">${timelineRead.error}</p>`}
        </div>
        <div class="tabs" role="tablist" aria-label="Zakładki">
          ${TABS.map(([key, label]) => html`
            <button role="tab" id=${`tab-${key}`} aria-selected=${tab === key} aria-controls="tab-panel"
              class=${`tab${tab === key ? ' active' : ''}`} onClick=${() => store.set({ tab: key })}>${label}</button>`)}
        </div>
      </div>
      <div class="tab-panel" id="tab-panel" role="tabpanel" aria-labelledby=${`tab-${tab}`}>${content}</div>
      <${AnswerPanel} store=${store} task=${task} onActions=${onActions} />
    </section>`;
}
