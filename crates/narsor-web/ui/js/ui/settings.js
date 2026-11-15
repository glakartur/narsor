// U10: history, repositories, the default harness and the task limit. Each
// is a dialog over the shell, as in the TUI (`H`, `R`, `h`, `L`).

import { html, useEffect, useRef, useState } from '../lib.js';
import { shortIdentifier, stateIcon, titleOf } from '../model.js';
import { clock } from '../text.js';
import { Field } from './fields.js';
import { Info } from './tabs/info.js';

function Dialog({ title, onClose, children, wide = false }) {
  const ref = useRef(null);
  useEffect(() => { ref.current?.focus(); }, []);
  return html`<div class="overlay" onClick=${onClose}>
    <div class=${`dialog${wide ? ' wide-dialog' : ''}`} role="dialog" aria-modal="true" aria-label=${title} tabindex="-1" ref=${ref}
      onClick=${(event) => event.stopPropagation()}
      onKeyDown=${(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }}>
      <div class="dialog-head"><h2>${title}</h2><button class="icon" onClick=${onClose} aria-label="Zamknij">✕</button></div>
      ${children}
    </div>
  </div>`;
}

function useLoad(load, deps = []) {
  const [state, setState] = useState({ loading: true });
  const reload = () => {
    setState((old) => ({ ...old, loading: true }));
    load().then((data) => setState({ data }), (error) => setState({ error: error.message }));
  };
  useEffect(reload, deps);
  return [state, reload];
}

// ---------------------------------------------------------------------------

export function History({ store, onClose, onOpen }) {
  const api = store.api;
  const [archived] = useLoad(() => api.tasks(true));
  const [legacy, reloadLegacy] = useLoad(() => api.capabilities().then((caps) => (caps.legacy_executions ? api.executions() : { executions: [] })));
  const [recovering, setRecovering] = useState(null);
  const [comment, setComment] = useState('');

  const recover = async (execution) => {
    try {
      const result = await store.mutate(
        (client) => client.recover(execution.execution_id ?? execution.id, { comment: comment.trim() || null }),
        { success: 'odzyskiwanie zlecone', failure: 'nie odzyskano' },
      );
      if (result !== undefined) { setRecovering(null); setComment(''); reloadLegacy(); }
    } catch { /* shown */ }
  };

  const tasks = archived.data?.tasks ?? [];
  const executions = legacy.data?.executions ?? [];
  return html`<${Dialog} title="Historia" onClose=${onClose} wide>
    <section>
      <h3>Zadania zarchiwizowane</h3>
      ${archived.loading && html`<p class="muted">wczytuję…</p>`}
      ${archived.error && html`<p class="role-problem">${archived.error}</p>`}
      ${!archived.loading && !tasks.length && html`<p class="muted">brak</p>`}
      <ul class="history-list">
        ${tasks.map((task) => html`<li key=${task.task_id}>
          <button class="action-item" onClick=${() => onOpen(task)}>
            <span class="action-label"><span class=${`role-${stateIcon(task).role}`}>${stateIcon(task).glyph}</span> ${titleOf(task)}</span>
            <span class="muted small-text">${shortIdentifier(task)} · ${task.workflow_id} v${task.workflow_version}${task.archive?.completed_at ? ` · ${new Date(task.archive.completed_at).toLocaleString('pl-PL')}` : ''}</span>
          </button></li>`)}
      </ul>
    </section>
    ${executions.length > 0 && html`<section>
      <h3>Wykonania sprzed harmonogramu zadań</h3>
      <ul class="history-list">
        ${executions.map((execution) => {
          const id = execution.execution_id ?? execution.id;
          return html`<li key=${id} class="card">
            <div><strong>${execution.workflow_id ?? id}</strong> <span class="muted small-text">${id} · ${execution.status ?? ''}${execution.started_at ? ` · ${clock(execution.started_at)}` : ''}</span></div>
            ${execution.problem && html`<span class="role-problem small-text">${execution.problem.kind}: ${execution.problem.reason}</span>`}
            ${!execution.recoverable && execution.not_recoverable_reason && html`<span class="muted small-text">${execution.not_recoverable_reason}</span>`}
            ${execution.recoverable && (recovering === id
              ? html`<div class="row"><input type="text" placeholder="komentarz (opcjonalnie)" value=${comment} onInput=${(event) => setComment(event.currentTarget.value)} />
                  <button class="primary" onClick=${() => recover(execution)}>Odzyskaj</button><button onClick=${() => setRecovering(null)}>Anuluj</button></div>`
              : html`<div><button class="small" onClick=${() => setRecovering(id)}>Odzyskaj…</button></div>`)}
          </li>`;
        })}
      </ul>
    </section>`}
  </${Dialog}>`;
}

// ---------------------------------------------------------------------------

const REPO_FIELDS = [
  ['id', true, 'identyfikator, np. narsor'],
  ['remote', true, 'adres git (https://, ssh://, git@host:ścieżka)'],
  ['name', false, null],
  ['provider', false, 'np. gitlab, github'],
  ['default_branch', false, null],
  ['description', false, null],
];

export function Repositories({ store, onClose }) {
  const api = store.api;
  const [list, reload] = useLoad(() => api.repositories());
  const [adding, setAdding] = useState(false);
  const [texts, setTexts] = useState({});
  const [problems, setProblems] = useState({});
  const [removing, setRemoving] = useState(null);

  const add = async (event) => {
    event.preventDefault();
    const found = {};
    for (const [name, required] of REPO_FIELDS) if (required && !(texts[name] ?? '').trim()) found[name] = `Pole '${name}' jest wymagane.`;
    setProblems(found);
    if (Object.keys(found).length) return;
    const body = Object.fromEntries(REPO_FIELDS.map(([name]) => [name, (texts[name] ?? '').trim() || null]));
    try {
      const result = await store.mutate((client) => client.addRepository(body), { success: `repozytorium ${body.id} dodane`, failure: 'nie dodano' });
      if (result !== undefined) { setAdding(false); setTexts({}); reload(); }
    } catch { /* shown */ }
  };

  const remove = async (id) => {
    try {
      const result = await store.mutate((client) => client.removeRepository(id), { success: `repozytorium ${id} usunięte`, failure: 'nie usunięto' });
      if (result !== undefined) { setRemoving(null); reload(); }
    } catch { /* shown */ }
  };

  const repos = list.data?.repositories ?? [];
  return html`<${Dialog} title="Repozytoria" onClose=${onClose} wide>
    ${list.loading && html`<p class="muted">wczytuję…</p>`}
    ${list.error && html`<p class="role-problem">${list.error}</p>`}
    ${!list.loading && !repos.length && html`<p class="muted">brak zarejestrowanych repozytoriów</p>`}
    <ul class="history-list">
      ${repos.map((repo) => html`<li key=${repo.id} class="card">
        <div><strong>${repo.id}</strong>${repo.name && repo.name !== repo.id ? html` <span class="muted">${repo.name}</span>` : ''}
          <span class="muted small-text"> · ${repo.provider ?? ''}${repo.default_branch ? ` · ${repo.default_branch}` : ''}</span></div>
        <code class="small-text">${repo.remote}</code>
        ${repo.description && html`<span class="muted small-text">${repo.description}</span>`}
        <div class="buttons">${removing === repo.id
          ? html`<span class="role-problem small-text">Usunąć ${repo.id}?</span><button class="danger small" onClick=${() => remove(repo.id)}>Usuń</button><button class="small" onClick=${() => setRemoving(null)}>Anuluj</button>`
          : html`<button class="small" onClick=${() => setRemoving(repo.id)}>Usuń…</button>`}</div>
      </li>`)}
    </ul>
    ${(list.data?.unreadable ?? []).map((line) => html`<p class="role-attention small-text">nieczytelne: ${line}</p>`)}
    ${adding
      ? html`<form class="answer-form" onSubmit=${add}>
          ${REPO_FIELDS.map(([name, required, hint], i) => html`<${Field} id=${`repo-${i}`} name=${name} kind=${{ type: 'text', options: [] }}
            required=${required} description=${hint} problem=${problems[name]} value=${texts[name] ?? ''}
            onInput=${(text) => setTexts((old) => ({ ...old, [name]: text }))} />`)}
          <div class="buttons"><button type="submit" class="primary">Dodaj</button><button type="button" onClick=${() => setAdding(false)}>Anuluj</button></div>
        </form>`
      : html`<div class="buttons"><button class="primary" onClick=${() => setAdding(true)}>+ Dodaj repozytorium</button></div>`}
  </${Dialog}>`;
}

// ---------------------------------------------------------------------------

export function Harness({ store, onClose }) {
  const snapshot = store.state.harness;
  const choose = async (harness) => {
    store.set({ harnessPending: harness ?? `${snapshot?.configured ?? ''}` });
    try {
      const result = await store.mutate(
        (client) => (harness ? client.setHarness(harness) : client.clearHarness()),
        { success: harness ? `domyślny harness: ${harness}` : 'harness z konfiguracji', failure: 'nie zmieniono harnessu' },
      );
      if (result !== undefined) { store.set({ harness: result, harnessPending: null }); onClose(); } else store.set({ harnessPending: null });
    } catch { store.set({ harnessPending: null }); }
  };
  if (!snapshot) return html`<${Dialog} title="Domyślny harness" onClose=${onClose}><p class="muted">brak danych z serwera</p></${Dialog}>`;
  const current = snapshot.override ?? null;
  return html`<${Dialog} title="Domyślny harness" onClose=${onClose}>
    <ul class="action-list">
      <li><button class=${`action-item${current === null ? ' chosen' : ''}`} onClick=${() => choose(null)}>
        <span class="action-label">domyślny z konfiguracji (${snapshot.configured})${current === null ? ' ✓' : ''}</span></button></li>
      ${(snapshot.supported ?? []).map((harness) => html`<li><button class=${`action-item${current === harness ? ' chosen' : ''}`} onClick=${() => choose(harness)}>
        <span class="action-label">${harness}${current === harness ? ' ✓' : ''}</span></button></li>`)}
    </ul>
  </${Dialog}>`;
}

// ---------------------------------------------------------------------------

export function Limit({ store, onClose }) {
  const api = store.api;
  const [view, reload] = useLoad(() => api.concurrencyLimit());
  const change = async (limit) => {
    try {
      const result = await store.mutate((client) => client.setConcurrencyLimit(limit), { success: `limit zadań: ${limit}`, failure: 'nie zmieniono limitu' });
      if (result !== undefined) reload();
    } catch { /* shown */ }
  };
  const data = view.data;
  return html`<${Dialog} title="Limit zadań" onClose=${onClose}>
    ${view.loading && !data && html`<p class="muted">wczytuję…</p>`}
    ${view.error && html`<p class="role-problem">${view.error}</p>`}
    ${data && html`
      <dl class="facts">
        <dt>limit</dt><dd><strong>${data.limit ?? 'bez limitu'}</strong></dd>
        <dt>zajęte sloty</dt><dd>${data.slots_held}</dd>
        ${data.overshoot > 0 && html`<dt>ponad limit</dt><dd class="role-attention">${data.overshoot}</dd>`}
        ${data.waiting_to_stop > 0 && html`<dt>czeka na zatrzymanie</dt><dd>${data.waiting_to_stop}</dd>`}
      </dl>
      <div class="buttons">
        <button disabled=${!data.limit || data.limit <= 1} onClick=${() => change(data.limit - 1)}>− 1</button>
        <button onClick=${() => change((data.limit ?? data.slots_held) + 1)}>+ 1</button>
      </div>
      <p class="muted small-text">Zmiana trwa do restartu serwera. Potem obowiązuje <code>max_concurrent_tasks</code> z konfiguracji.</p>`}
  </${Dialog}>`;
}

/** An archived task: only its record is left (storage removed). */
export function ArchivedTask({ task, onClose }) {
  return html`<${Dialog} title=${titleOf(task)} onClose=${onClose} wide>
    <p class="muted">Zadanie zarchiwizowane — storage usunięty przy archiwizacji.</p>
    <${Info} task=${task} />
  </${Dialog}>`;
}
