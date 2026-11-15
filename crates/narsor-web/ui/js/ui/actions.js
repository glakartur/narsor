// The action panel: what the owner can do with the task now, as the server
// offers it (`TaskSummary.actions`). Three steps, as in the TUI: choose
// (a submenu opens in place), fill the fields (skipped when there are none),
// confirm. The client has no list of its own and no switch on action ids.

import { html, useEffect, useRef, useState } from '../lib.js';
import { pullRequest } from '../model.js';
import { panelActions, resolveSend } from '../panel.js';
import { Field } from './fields.js';

function kindOfField(field) {
  if (field.kind === 'select') return { type: 'select', options: field.options ?? [] };
  return { type: field.kind === 'multiline' ? 'multiline' : 'text', options: [] };
}

function send(store, task, action, texts) {
  const request = resolveSend(action.send, texts);
  const gen = task.generation;
  const run = {
    owner: (api) => api.command(task.task_id, request.body, gen),
    operator: (api) => api.operatorCommand(task.task_id, request.body, gen),
    comment: (api) => api.comment(task.task_id, request.body, gen),
  }[request.route];
  return store.mutate(run, { success: `${action.label}: wysłano`, failure: `${action.label}: odrzucone` });
}

export function ActionPanel({ store, task, onClose, onAnswer }) {
  const top = panelActions(task);
  const [path, setPath] = useState([]);
  const [action, setAction] = useState(null);
  const [step, setStep] = useState('choose');
  const [texts, setTexts] = useState({});
  const [problems, setProblems] = useState({});
  const [error, setError] = useState(null);
  const [sending, setSending] = useState(false);
  const ref = useRef(null);
  useEffect(() => { ref.current?.focus(); }, [step, path.length]);

  const list = path.length ? path[path.length - 1].children ?? [] : top ?? [];
  const sorted = [...list].sort((a, b) => (a.order ?? 0) - (b.order ?? 0));
  const pr = (task.workspaces ?? []).map(pullRequest).find((item) => item?.url);

  const choose = (item) => {
    setError(null);
    if (item.available === false) return;
    if (item.kind === 'submenu') { setPath([...path, item]); return; }
    if (!item.send) {
      // An action the client answers itself (the answer panel).
      onClose();
      onAnswer?.();
      return;
    }
    setAction(item);
    setTexts(Object.fromEntries((item.fields ?? []).map((field) => [field.name, ''])));
    setProblems({});
    setStep(item.fields?.length ? 'fields' : 'confirm');
  };

  const toConfirm = (event) => {
    event.preventDefault();
    const found = {};
    for (const field of action.fields ?? []) {
      if (field.required && !(texts[field.name] ?? '').trim()) found[field.name] = `Pole '${field.name}' jest wymagane.`;
    }
    setProblems(found);
    if (!Object.keys(found).length) setStep('confirm');
  };

  const confirm = async () => {
    setError(null);
    setSending(true);
    try {
      const result = await send(store, task, action, texts);
      if (result !== undefined) onClose();
    } catch (problem) {
      if (!problem?.kind) setError(problem.message);
    } finally {
      setSending(false);
    }
  };

  const back = () => {
    setError(null);
    if (step === 'confirm' && action?.fields?.length) setStep('fields');
    else if (step !== 'choose') { setStep('choose'); setAction(null); }
    else if (path.length) setPath(path.slice(0, -1));
    else onClose();
  };

  let body;
  if (top === null) body = html`<p class="muted">Brak danych z serwera.</p>`;
  else if (step === 'choose') {
    body = sorted.length === 0 ? html`<p class="muted">brak dostępnych akcji</p>` : html`
      <ul class="action-list">
        ${sorted.map((item, i) => html`<li key=${`${item.id}-${item.label}-${i}`}>
          <button class="action-item" disabled=${item.available === false} onClick=${() => choose(item)}>
            <span class="action-label">${item.label}${item.kind === 'submenu' ? ' ▸' : ''}${item.irreversible ? html` <span class="role-problem small-text">! nieodwracalne</span>` : ''}</span>
            ${item.description && html`<span class="muted small-text">${item.description}</span>`}
            ${item.available === false && item.reason && html`<span class="role-attention small-text">${item.reason}</span>`}
          </button>
        </li>`)}
        ${pr && !path.length && html`<li><a class="action-item" href=${pr.url} target="_blank" rel="noopener noreferrer">
          <span class="action-label">Otwórz link ${pr.label} ↗</span></a></li>`}
      </ul>`;
  } else if (step === 'fields') {
    body = html`<form onSubmit=${toConfirm}>
      ${(action.fields ?? []).map((field, i) => html`<${Field} id=${`action-field-${i}`} name=${field.name}
        kind=${kindOfField(field)} required=${field.required} description=${field.hint} problem=${problems[field.name]}
        value=${texts[field.name] ?? ''} onInput=${(text) => { setTexts({ ...texts, [field.name]: text }); setProblems({ ...problems, [field.name]: null }); }} />`)}
      <div class="buttons"><button type="submit" class="primary">Dalej</button></div>
    </form>`;
  } else {
    const filled = (action.fields ?? []).filter((field) => (texts[field.name] ?? '').trim());
    body = html`<div class="confirm">
      <p><strong>${action.label}</strong>${action.irreversible ? html` <span class="role-problem">! nieodwracalne</span>` : ''}</p>
      ${action.description && html`<p class="muted">${action.description}</p>`}
      ${action.effect && html`<p>${action.effect}</p>`}
      ${filled.length > 0 && html`<dl class="facts">${filled.map((field) => html`<dt>${field.name}</dt><dd><pre class="msg-text">${texts[field.name]}</pre></dd>`)}</dl>`}
      ${error && html`<p class="role-problem" role="alert">${error}</p>`}
      <div class="buttons">
        <button class=${action.irreversible ? 'danger' : 'primary'} disabled=${sending} onClick=${confirm}>${sending ? 'wysyłanie…' : 'Potwierdź'}</button>
        <button onClick=${back}>Wróć</button>
      </div>
    </div>`;
  }

  const title = step === 'choose'
    ? (path.length ? path.map((item) => item.label).join(' ▸ ') : 'Akcje')
    : action.label;
  return html`
    <div class="overlay" onClick=${onClose}>
      <div class="dialog actions-dialog" role="dialog" aria-modal="true" aria-labelledby="actions-title" tabindex="-1" ref=${ref}
        onClick=${(event) => event.stopPropagation()}
        onKeyDown=${(event) => { if (event.key === 'Escape') { event.stopPropagation(); back(); } }}>
        <div class="dialog-head">
          ${(path.length > 0 || step !== 'choose') && html`<button class="small" onClick=${back} aria-label="Wstecz">←</button>`}
          <h2 id="actions-title">${title}</h2>
          <button class="icon" onClick=${onClose} aria-label="Zamknij">✕</button>
        </div>
        ${body}
      </div>
    </div>`;
}
