// The panel „Czeka na Ciebie": the forms the server offers for the task
// (`TaskSummary.panel`). A form with a definition sends one of its entries
// through `POST …/input`; the chat form sends a comment; the form of the
// task's own actions opens the action panel. Each form keeps its own draft,
// which outlives a refresh and a change of the selected task.

import { html, useEffect, useState } from '../lib.js';
import {
  checkEntry, deliver, fieldKind, formKey, formName, initialValue, panelForms,
} from '../panel.js';
import { Field } from './fields.js';

/** Drafts by form key: `{entry, texts: {field: text}}`. */
const drafts = new Map();

/** Whether any form has text the operator typed and did not send. */
export function hasUnsentDraft() {
  for (const draft of drafts.values()) {
    if (Object.values(draft.texts ?? {}).some((text) => text.trim())) return true;
  }
  return false;
}

function useDraft(key, initial) {
  const [draft, setDraft] = useState(() => drafts.get(key) ?? initial);
  useEffect(() => setDraft(drafts.get(key) ?? initial), [key]);
  const update = (next) => setDraft((old) => {
    const value = typeof next === 'function' ? next(old) : next;
    drafts.set(key, value);
    return value;
  });
  const clear = () => {
    drafts.delete(key);
    setDraft(initial);
  };
  return [draft, update, clear];
}

function Context({ lines }) {
  if (!lines?.length) return null;
  return html`<div class="panel-context">
    ${lines.map((line) => html`<pre class=${`msg-text tone-${line.tone}`}>${line.tone === 'conductor' ? '◆ ' : ''}${line.text}</pre>`)}
  </div>`;
}

function DefinitionForm({ store, task, form, formId }) {
  const definition = form.definition;
  const entries = definition.entries ?? [];
  const firstRecommended = Math.max(0, entries.findIndex((entry) => entry.recommended));
  const [draft, setDraft, clearDraft] = useDraft(formId, { entry: firstRecommended, texts: {} });
  const [problems, setProblems] = useState({});
  const [error, setError] = useState(null);
  const [sending, setSending] = useState(false);
  const index = Math.min(draft.entry, entries.length - 1);
  const entry = entries[index];

  const choose = (next) => {
    setDraft((old) => ({ ...old, entry: next }));
    setProblems({});
    setError(null);
  };
  const setText = (name, text) => {
    setDraft((old) => ({ ...old, texts: { ...old.texts, [name]: text } }));
    setProblems((old) => ({ ...old, [name]: null }));
  };

  const send = async (event) => {
    event.preventDefault();
    setError(null);
    const { problems: found, values } = checkEntry(entry, draft.texts);
    setProblems(found);
    if (Object.keys(found).length) return;
    let delivery;
    try {
      delivery = deliver(definition, index, values);
    } catch (problem) {
      setError(problem.message);
      return;
    }
    setSending(true);
    try {
      const result = await store.mutate(
        (api) => api.input(task.task_id, definition.episode, delivery),
        { success: entry.summary || `odpowiedziano ${entry.label}`, failure: 'odpowiedź odrzucona' },
      );
      if (result !== undefined) clearDraft();
    } catch {
      /* the text stays; the store showed the reason */
    } finally {
      setSending(false);
    }
  };

  if (!entry) return html`<p class="muted">Formularz nie ma pozycji.</p>`;
  return html`<form class="answer-form" onSubmit=${send}>
    <${Context} lines=${definition.context} />
    <div class="entries" role="radiogroup" aria-label="Wybierz odpowiedź">
      ${entries.map((item, i) => html`
        <label class=${`entry-choice${i === index ? ' chosen' : ''}`}>
          <input type="radio" name=${`${formId}-entry`} checked=${i === index} onChange=${() => choose(i)} />
          <span>
            <strong>${item.label}</strong>
            ${item.recommended && html` <span class="badge">zalecane</span>`}
            ${item.resumes && html` <span class="muted small-text">↩ wraca do ${item.resumes}</span>`}
            ${item.description && html`<span class="muted small-text entry-desc">${item.description}</span>`}
          </span>
        </label>`)}
    </div>
    ${(entry.fields ?? []).map((field, i) => html`
      <${Field} id=${`${formId}-${index}-${i}`} name=${field.name} kind=${fieldKind(field)} required=${field.required}
        description=${field.description} problem=${problems[field.name]}
        value=${draft.texts[field.name] ?? initialValue(field)} onInput=${(text) => setText(field.name, text)} />`)}
    ${error && html`<p class="role-problem" role="alert">${error}</p>`}
    <div class="buttons">
      <button type="submit" class="primary" disabled=${sending}>${sending ? 'wysyłanie…' : `Wyślij: ${entry.label}`}</button>
    </div>
  </form>`;
}

function ChatForm({ store, task, formId }) {
  const [draft, setDraft, clearDraft] = useDraft(formId, { texts: { text: '' } });
  const [sending, setSending] = useState(false);
  const text = draft.texts.text ?? '';
  const send = async (event) => {
    event.preventDefault();
    if (!text.trim()) return;
    setSending(true);
    try {
      const result = await store.mutate(
        (api) => api.comment(task.task_id, text.trim(), task.generation),
        { success: 'wiadomość wysłana', failure: 'wiadomość odrzucona' },
      );
      if (result !== undefined) clearDraft();
    } catch {
      /* the text stays */
    } finally {
      setSending(false);
    }
  };
  return html`<form class="answer-form" onSubmit=${send}>
    <textarea rows="3" aria-label="Wiadomość" placeholder="Napisz wiadomość do zadania…" value=${text}
      onInput=${(event) => setDraft({ texts: { text: event.currentTarget.value } })}
      onKeyDown=${(event) => { if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) send(event); }}></textarea>
    <div class="buttons"><button type="submit" class="primary" disabled=${sending || !text.trim()}>Wyślij</button></div>
  </form>`;
}

export function AnswerPanel({ store, task, onActions }) {
  const { forms, initial } = panelForms(task);
  const [chosen, setChosen] = useState(null);
  const [collapsed, setCollapsed] = useState(false);
  useEffect(() => setChosen(null), [task.task_id]);
  if (!forms.length) return null;
  const index = chosen !== null && chosen < forms.length ? chosen : initial;
  const form = forms[index];
  const formId = formKey(task.task_id, form).replace(/[^A-Za-z0-9_-]/g, '_');
  let body;
  if (form.definition) body = html`<${DefinitionForm} key=${formId} store=${store} task=${task} form=${form} formId=${formId} />`;
  else if (form.kind === 'chat') body = html`<${ChatForm} key=${formId} store=${store} task=${task} formId=${formId} />`;
  else body = html`<div class="buttons"><button class="primary" onClick=${onActions}>Otwórz akcje (.)</button></div>`;
  return html`<section class="answer-panel" aria-label="Czeka na Ciebie">
    <div class="answer-head">
      <button class="link-head" aria-expanded=${!collapsed} onClick=${() => setCollapsed(!collapsed)}>
        <span class="role-attention">?</span> Czeka na Ciebie ${collapsed ? '▸' : '▾'}
      </button>
      ${forms.length > 1 && html`<div class="switcher" role="tablist" aria-label="Formularze">
        ${forms.map((item, i) => html`<button role="tab" aria-selected=${i === index} class=${`small${i === index ? ' active' : ''}`}
          onClick=${() => { setChosen(i); setCollapsed(false); }}>${formName(item)}</button>`)}
      </div>`}
    </div>
    ${!collapsed && html`<div class="answer-body">${body}</div>`}
  </section>`;
}
