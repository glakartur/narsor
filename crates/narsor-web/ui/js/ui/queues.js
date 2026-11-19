// U11: the automation queues, read only. Automations and queues are files of
// the catalogue; Narsor only reads them, so the page has no forms. The list
// of queues with their counts and the catalogue files that did not load; a
// queue page with its automations (scripts, state, last run, recent errors),
// its entries and the problems of its file. A stopped automation runs again
// when its queue file, `automation.yaml` or `check` script changes in the
// catalogue.

import { html, useEffect, useRef, useState } from '../lib.js';
import { shortId } from '../model.js';
import { refusalReason } from '../store.js';
import {
  automationState, dateTime, entryAutomationParts, firstLine, lastRunText, modeText, scheduleText,
} from '../text.js';

/**
 * `load()` again whenever `key` changes; the last data stays while a new
 * read runs, so a refresh does not blank the page.
 */
function useLoaded(load, key) {
  const [result, setResult] = useState({ data: null, error: null, loading: true });
  const latest = useRef(load);
  latest.current = load;
  useEffect(() => {
    let live = true;
    setResult((old) => ({ ...old, loading: true }));
    latest.current().then(
      (data) => { if (live) setResult({ data, error: null, loading: false }); },
      (error) => { if (live) setResult((old) => ({ data: old.data, error, loading: false })); },
    );
    return () => { live = false; };
  }, [key]);
  return result;
}

/** The page: the list of queues, or one queue (`state.queue`). */
export function Queues({ store, state, onOpenTask }) {
  // The task list is read again after every stream event (debounced) and
  // every 10 s: the queues follow it, since entries come and go with tasks.
  const tick = state.tasks;
  const known = new Set((state.tasks ?? []).map((task) => task.task_id));
  return html`<main class="queues-page" aria-label="Kolejki automatyzacji">
    ${state.queue === null
      ? html`<${QueueList} store=${store} tick=${tick} />`
      : html`<${QueuePage} key=${state.queue} store=${store} queue=${state.queue} tick=${tick} known=${known}
          onOpenTask=${onOpenTask} />`}
  </main>`;
}

function Spinner({ read }) {
  return read.loading && read.data ? html` <span class="spinner" aria-hidden="true">⠋</span>` : '';
}

/** The read state of a page: loading, or the refusal of the read. */
function ReadState({ read }) {
  return html`
    ${read.loading && !read.data && html`<p class="muted">wczytuję…</p>`}
    ${read.error && html`<p class="role-problem">${refusalReason(read.error)}</p>`}`;
}

/** The catalogue files that did not load (`[{file, reason}]`). */
export function Problems({ problems, title }) {
  if (!problems?.length) return null;
  return html`<h3 class="role-problem">${title} (${problems.length})</h3>
    <ul class="entry-observers">
      ${problems.map((problem) => html`<li key=${problem.file}>
        <span class="mono">${problem.file}</span><span class="muted"> · </span><span class="role-problem">${problem.reason}</span>
      </li>`)}
    </ul>`;
}

// ---------------------------------------------------------------------------
// The list of queues

function QueueList({ store, tick }) {
  const read = useLoaded(() => store.api.observerQueues(), tick);
  return html`<section class="pane queues-pane">
    <div class="pane-head">
      <button class="small" onClick=${() => store.openTasks()} aria-label="Wróć do zadań">←</button>
      <h2 class="details-title">Kolejki automatyzacji<${Spinner} read=${read} /></h2>
    </div>
    <div class="queues-body">
      <${ReadState} read=${read} />
      ${read.data && html`<${QueueTable} list=${read.data} onOpen=${(queue) => store.openQueues(queue)} />`}
    </div>
  </section>`;
}

/** The queues of the catalogue and the files that did not load. */
export function QueueTable({ list, onOpen }) {
  const queues = list.queues ?? [];
  return html`
    ${!queues.length && html`<p class="empty">brak kolejek — dodaj kolejkę do katalogu albo uruchom workflow z bramką <code>watch</code></p>`}
    ${queues.length > 0 && html`<table class="event-table queue-table">
      <thead><tr><th>kolejka</th><th>tryb</th><th>harmonogram</th><th>automatyzacje</th><th class="num">wpisy</th></tr></thead>
      <tbody>
        ${queues.map((queue) => html`<tr key=${queue.queue}>
          <td>
            <button class="link" onClick=${() => onOpen(queue.queue)}>${queue.queue}</button>
            ${queue.problem && html` <span class="role-problem" title=${queue.problem} aria-label=${`problem: ${queue.problem}`}>!</span>`}
          </td>
          <td class="muted">${queue.mode ?? '—'}</td>
          <td class="muted">${scheduleText(queue)}</td>
          <td class="mono">${(queue.automations ?? []).join(', ') || '—'}</td>
          <td class="num"><strong>${queue.entries}</strong></td>
        </tr>`)}
      </tbody>
    </table>`}
    <${Problems} problems=${list.problems} title="Pliki katalogu, których nie wczytano" />`;
}

// ---------------------------------------------------------------------------
// One queue

function QueuePage({ store, queue, tick, known, onOpenTask }) {
  const read = useLoaded(() => store.api.observerQueue(queue), tick);
  return html`<section class="pane queues-pane">
    <div class="pane-head">
      <button class="small" onClick=${() => store.openQueues(null)} aria-label="Wróć do listy kolejek">←</button>
      <h2 class="details-title">Kolejka <span class="mono">${queue}</span><${Spinner} read=${read} /></h2>
    </div>
    <div class="queues-body">
      <${ReadState} read=${read} />
      ${read.data && html`<${QueueDetail} view=${read.data} known=${known} onOpenTask=${onOpenTask} />`}
    </div>
  </section>`;
}

/** One queue: its settings, the problems of its file, its automations and its entries. */
export function QueueDetail({ view, known = new Set(), onOpenTask = () => {} }) {
  const automations = view.automations ?? [];
  const entries = view.entries ?? [];
  return html`
    <dl class="facts compact">
      <dt>tryb</dt><dd>${modeText(view.mode)}</dd>
      <dt>harmonogram</dt><dd>${scheduleText(view)}</dd>
      ${view.workflow_id && html`<dt>workflow</dt><dd class="mono">${view.workflow_id}</dd>`}
    </dl>
    <${Problems} problems=${view.problems} title="Problemy pliku kolejki" />
    <h3>Automatyzacje (${automations.length})</h3>
    ${!automations.length && html`<p class="muted">brak — wpisy czekają na decyzję osoby</p>`}
    ${automations.map((automation) => html`<${AutomationCard} key=${automation.automation} automation=${automation} />`)}
    <h3>Wpisy (${entries.length})</h3>
    ${!entries.length && html`<p class="muted">brak zadań czekających na tej kolejce</p>`}
    ${entries.map((entry) => html`<${EntryCard} key=${entry.entry_id} entry=${entry} known=${known.has(entry.task_id)} onOpenTask=${onOpenTask} />`)}`;
}

function AutomationCard({ automation }) {
  const state = automationState(automation.state);
  const lastRun = lastRunText(automation.last_run);
  const secrets = automation.secrets ?? [];
  const errors = automation.errors ?? [];
  const has = (value) => value !== undefined && value !== null;
  return html`<article class="card observer-card" aria-label=${`Automatyzacja ${automation.automation}`}>
    <div class="observer-head">
      <strong>${automation.automation}</strong>
      <span class=${`role-${state.role}`}>${state.text}</span>
      ${automation.description && html`<span class="muted small-text">${automation.description}</span>`}
    </div>
    <dl class="facts compact">
      <dt>skrypty</dt><dd class="mono">${(automation.scripts ?? []).join(', ') || '—'}</dd>
      ${secrets.length > 0 && html`<dt>sekrety</dt><dd class="mono">${secrets.map((secret) => `${secret.name} → $${secret.env}`).join(', ')}</dd>`}
      <dt>ostatni przebieg</dt><dd class=${automation.last_run?.error ? 'role-problem' : ''}>
        ${lastRun ?? 'jeszcze nie było'}${automation.last_run?.error ? ` — ${firstLine(automation.last_run.error, 160)}` : ''}</dd>
      ${has(automation.active_run) && html`<dt>trwa</dt><dd class="role-live">przebieg #${automation.active_run}</dd>`}
      ${automation.next_run_at && html`<dt>następny</dt><dd>${dateTime(automation.next_run_at)}</dd>`}
      ${automation.last_claim_at && html`<dt>ostatnio pobrany</dt><dd>${dateTime(automation.last_claim_at)}</dd>`}
      ${automation.waiting_for_claim_since && html`<dt>czeka na workera od</dt><dd>${dateTime(automation.waiting_for_claim_since)}</dd>`}
      ${automation.consecutive_failures > 0 && html`<dt>porażki z rzędu</dt><dd class="role-problem">${automation.consecutive_failures}</dd>`}
    </dl>
    ${automation.worker_missing && html`<p class="role-attention small-text">przebieg czeka na workera dłużej niż jeden interwał — czy narsor-observer-worker działa?</p>`}
    ${state.stopped && html`<p class="role-attention small-text">zatrzymana — ruszy ponownie, gdy w katalogu zmieni się plik kolejki, <code>automation.yaml</code> lub skrypt <code>check</code></p>`}
    ${errors.length > 0 && html`<details class="cli-settings">
      <summary>ostatnie błędy (${errors.length})</summary>
      ${errors.map((error) => html`<div key=${error.run_id}>
        <p class="role-problem small-text">#${error.sequence} ${error.phase} · ${error.status}${has(error.exit_code) ? `, kod ${error.exit_code}` : ''}
          · ${dateTime(error.finished_at)}${error.entry_id ? ` · wpis ${shortId(error.entry_id)}` : ''}: ${firstLine(error.error, 200)}</p>
        ${error.stderr && html`<pre class="script-text" aria-label="stderr">${error.stderr}</pre>`}
        ${error.stdout && html`<pre class="script-text" aria-label="stdout">${error.stdout}</pre>`}
      </div>`)}
    </details>`}
  </article>`;
}

function EntryCard({ entry, known, onOpenTask }) {
  const params = Object.entries(entry.params ?? {});
  return html`<article class="card entry-card" aria-label=${`Wpis zadania ${entry.task_id}`}>
    <div class="observer-head">
      ${known
        ? html`<button class="link mono" title=${entry.task_id} onClick=${() => onOpenTask(entry.task_id)}>id:${shortId(entry.task_id)}</button>`
        : html`<span class="mono" title=${entry.task_id}>id:${shortId(entry.task_id)}</span>`}
      <span>krok <strong>${entry.step}</strong></span>
      <span class="muted small-text">epizod <span class="mono" title=${entry.episode}>${firstLine(entry.episode, 24)}</span></span>
      <span class="muted small-text">od ${dateTime(entry.started_at)}</span>
    </div>
    ${params.length > 0 && html`<dl class="facts compact">
      ${params.map(([key, value]) => html`<dt class="mono">${key}</dt><dd class="mono">${firstLine(value, 200)}</dd>`)}
    </dl>`}
    <ul class="entry-observers">
      ${(entry.automations ?? []).map((view) => html`<li key=${view.automation}>
        <strong>${view.automation}</strong>
        ${entryAutomationParts(view).map((part) => html`<span class="muted"> · </span><span class=${`role-${part.role}`}>${part.text}</span>`)}
      </li>`)}
    </ul>
  </article>`;
}
