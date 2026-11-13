import { html, useEffect, useRef } from '../lib.js';
import {
  activityAge, activitySince, currentStep, isDone, needsOperator, queuePositions, secondsBetween,
  shortIdentifier, stateIcon, titleOf,
} from '../model.js';

function label(task, places) {
  if (task.state === 'queued') {
    const place = places.get(task.task_id);
    return place ? { text: `#${place}`, role: 'calm' } : null;
  }
  if (task.state === 'running') {
    const step = currentStep(task);
    return step ? { text: step, role: 'done' } : null;
  }
  return null;
}

function Row({ task, places, selected, stale, now, canMoveUp, canMoveDown, onSelect, onOpen, onMove }) {
  const icon = stateIcon(task);
  const since = activitySince(task);
  const age = since ? activityAge(secondsBetween(since, now)) : null;
  const tag = label(task, places);
  const loud = needsOperator(icon);
  const ref = useRef(null);
  useEffect(() => {
    if (selected) ref.current?.scrollIntoView({ block: 'nearest' });
  }, [selected]);
  const queued = task.runner_state === 'queued';
  return html`
    <li ref=${ref} role="option" aria-selected=${selected} id=${`task-${task.task_id}`}
      class=${`task-row${selected ? ' selected' : ''}${loud ? ' loud' : ''}${stale ? ' stale' : ''}`}
      onClick=${() => onSelect(task.task_id)} onDblClick=${() => onOpen(task.task_id)}>
      <span class=${`state-icon role-${icon.role}`} title=${icon.word} aria-label=${icon.word}>${icon.glyph}</span>
      <div class="task-text">
        <div class="task-id">${shortIdentifier(task)}</div>
        <div class="task-title">${titleOf(task)}</div>
        <div class="task-meta">
          ${tag && html`<span class=${`role-${tag.role}`}>${tag.text}</span>`}
          ${tag && age && html`<span class="muted"> · </span>`}
          ${age && html`<span class="muted">${age}</span>`}
          <span class="sr-only">, ${icon.word}</span>
        </div>
      </div>
      ${queued && html`
        <span class="move" onClick=${(event) => event.stopPropagation()}>
          <button class="icon small" title="Wyżej w kolejce" aria-label="Wyżej w kolejce" disabled=${!canMoveUp} onClick=${() => onMove(task, -1)}>▲</button>
          <button class="icon small" title="Niżej w kolejce" aria-label="Niżej w kolejce" disabled=${!canMoveDown} onClick=${() => onMove(task, 1)}>▼</button>
        </span>`}
    </li>`;
}

export function TaskList({ store, state, rows, now, filterRef, onOpen, onMove, onNew }) {
  const all = state.tasks ?? [];
  const places = queuePositions(all);
  const hidden = all.filter(isDone).length;
  const stale = state.connection.state !== 'live' && state.connection.lastData !== null;
  const queuedOf = (task) => rows.filter((row) => row.runner_state === 'queued' && row.backend_id === task.backend_id);
  let body;
  if (state.tasks === null) body = html`<p class="empty">wczytuję zadania…</p>`;
  else if (rows.length === 0) body = html`<p class="empty">${state.connection.state === 'live' ? 'brak zadań' : 'brak danych'}</p>`;
  else {
    body = html`
      <ul class="task-list" role="listbox" aria-label="Zadania" tabindex="0"
        aria-activedescendant=${state.selectedTaskId ? `task-${state.selectedTaskId}` : undefined}>
        ${rows.map((task) => {
          const queue = queuedOf(task);
          const index = queue.indexOf(task);
          return html`<${Row} key=${task.task_id} task=${task} places=${places} now=${now} stale=${stale}
            selected=${task.task_id === state.selectedTaskId}
            canMoveUp=${index > 0} canMoveDown=${index >= 0 && index < queue.length - 1}
            onSelect=${(id) => (window.matchMedia('(max-width: 900px)').matches ? onOpen(id) : store.select(id))} onOpen=${onOpen} onMove=${onMove} />`;
        })}
      </ul>`;
  }
  return html`
    <section class="pane list-pane" aria-label="Zadania">
      <div class="pane-head">
        <h2>Zadania${state.loading && state.tasks?.length ? html` <span class="spinner" aria-hidden="true">⠋</span>` : ''}</h2>
        <span class="buttons">
          <button class="small" title="Następne czekające (Spacja)" onClick=${() => store.selectNextWaiting()}>następne ?</button>
          <button class="small primary" title="Nowe zadanie (n)" onClick=${onNew}>+ Nowe</button>
        </span>
      </div>
      <div class="list-tools">
        <input ref=${filterRef} type="search" placeholder="Filtr (/)" aria-label="Filtr zadań" value=${state.filter}
          onInput=${(event) => store.setView({ filter: event.currentTarget.value })}
          onKeyDown=${(event) => { if (event.key === 'Escape' || event.key === 'Enter') event.currentTarget.blur(); }} />
        <label class="toggle" title="Ukryj zakończone (z)">
          <input type="checkbox" checked=${state.hideDone} onChange=${(event) => store.setView({ hideDone: event.currentTarget.checked })} />
          ukryj ✓${hidden ? ` (${hidden})` : ''}
        </label>
      </div>
      ${body}
    </section>`;
}
