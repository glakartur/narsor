// U11: the queues of the owner's observers (R23, R31–R34). The list of
// queues with their counts; a queue page with its observers and its entries;
// the forms that create an observer and change its queue, mode and schedule;
// the script editor (a save makes a new version and never approves it); the
// approve dialog that shows the script text first; pause, resume and delete.
// The settings the CLI sets (secrets, task limit, workflow mapping) are shown
// read only (R34). Everything comes from the owner routes the CLI calls.
// A dialog with script text that is not saved asks before it closes, and the
// tab asks before it closes too.

import { html, useEffect, useRef, useState } from '../lib.js';
import {
  OBSERVER_MODES, createRequest, observerForm, scheduleCommands, shortId, unapprovedVersion, updateRequest,
} from '../model.js';
import { refusalReason } from '../store.js';
import {
  dateTime, entryObserverParts, firstLine, lastRunText, modeText, observerState, scheduleText, unapprovedText,
  versionRows,
} from '../text.js';
import { Field } from './fields.js';
import { Dialog } from './settings.js';

/** The hint of the settings only the CLI changes (R34). */
export const CLI_HINT = 'Sekrety, limit zadań i mapowanie workflow ustawia CLI: narsor-cli observer create --secret NAME=ENV --task-limit N --workflow WF --map INPUT=FIELD.';

/**
 * `load()` again whenever `key` changes; the last data stays while a new
 * read runs, so a refresh does not blank the page.
 */
function useLoaded(load, key) {
  const [result, setResult] = useState({ data: null, error: null, loading: true });
  const [round, setRound] = useState(0);
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
  }, [key, round]);
  return { ...result, reload: () => setRound((value) => value + 1) };
}

/** The page: the list of queues, or one queue (`state.queue`). */
export function Queues({ store, state, onOpenTask }) {
  const [dialog, setDialog] = useState(null);
  // The task list is read again after every stream event (debounced) and
  // every 10 s: the queues follow it, since entries come and go with tasks.
  const tick = state.tasks;
  const close = () => setDialog(null);
  const known = new Set((state.tasks ?? []).map((task) => task.task_id));
  return html`<main class="queues-page" aria-label="Kolejki obserwatorów">
    ${state.queue === null
      ? html`<${QueueList} store=${store} tick=${tick} onDialog=${setDialog} />`
      : html`<${QueuePage} key=${state.queue} store=${store} queue=${state.queue} tick=${tick} known=${known}
          onDialog=${setDialog} onOpenTask=${onOpenTask} />`}
    ${dialog?.kind === 'create' && html`<${ObserverFormDialog} store=${store} defaults=${dialog.defaults} onClose=${close}
      onDone=${(queue) => { close(); store.openQueues(queue); }} />`}
    ${dialog?.kind === 'edit' && html`<${ObserverFormDialog} store=${store} observer=${dialog.observer} onClose=${close}
      onDone=${(queue) => { close(); store.openQueues(queue); }} />`}
    ${dialog?.kind === 'script' && html`<${ScriptEditor} store=${store} observer=${dialog.observer} onClose=${close} onDone=${close} />`}
    ${dialog?.kind === 'approve' && html`<${ApproveDialog} store=${store} observer=${dialog.observer} onClose=${close} onDone=${close} />`}
    ${dialog?.kind === 'delete' && html`<${DeleteDialog} store=${store} observer=${dialog.observer} onClose=${close} onDone=${close} />`}
  </main>`;
}

// ---------------------------------------------------------------------------
// The list of queues

function QueueList({ store, tick, onDialog }) {
  const read = useLoaded(() => store.api.observerQueues(), tick);
  const queues = read.data?.queues ?? [];
  return html`<section class="pane queues-pane">
    <div class="pane-head">
      <button class="small" onClick=${() => store.openTasks()} aria-label="Wróć do zadań">←</button>
      <h2 class="details-title">Kolejki obserwatorów${read.loading && read.data ? html` <span class="spinner" aria-hidden="true">⠋</span>` : ''}</h2>
      <button class="small primary" onClick=${() => onDialog({ kind: 'create', defaults: {} })}>+ Nowy obserwator</button>
    </div>
    <div class="queues-body">
      ${read.loading && !read.data && html`<p class="muted">wczytuję…</p>`}
      ${read.error && html`<p class="role-problem">${refusalReason(read.error)}</p>`}
      ${read.data && !queues.length && html`<p class="empty">brak kolejek — utwórz obserwatora albo uruchom workflow z bramką <code>watch</code></p>`}
      ${queues.length > 0 && html`<table class="event-table queue-table">
        <thead><tr><th>kolejka</th><th>tryb</th><th class="num">obserwatory</th><th class="num">wpisy</th></tr></thead>
        <tbody>
          ${queues.map((queue) => html`<tr key=${queue.queue}>
            <td><button class="link" onClick=${() => store.openQueues(queue.queue)}>${queue.queue}</button></td>
            <td class="muted">${queue.mode ?? 'brak obserwatorów'}</td>
            <td class="num">${queue.observers}</td>
            <td class="num"><strong>${queue.entries}</strong></td>
          </tr>`)}
        </tbody>
      </table>`}
    </div>
  </section>`;
}

// ---------------------------------------------------------------------------
// One queue

function QueuePage({ store, queue, tick, known, onDialog, onOpenTask }) {
  const read = useLoaded(() => store.api.observerQueue(queue), tick);
  const view = read.data;
  const gone = read.error?.kind === 'not_found';
  return html`<section class="pane queues-pane">
    <div class="pane-head">
      <button class="small" onClick=${() => store.openQueues(null)} aria-label="Wróć do listy kolejek">←</button>
      <h2 class="details-title">Kolejka <span class="mono">${queue}</span>${read.loading && view ? html` <span class="spinner" aria-hidden="true">⠋</span>` : ''}</h2>
      <button class="small primary" onClick=${() => onDialog({ kind: 'create', defaults: { queue, mode: view?.mode ?? undefined } })}>+ Nowy obserwator</button>
    </div>
    <div class="queues-body">
      ${read.loading && !view && html`<p class="muted">wczytuję…</p>`}
      ${gone && html`<p class="muted">tej kolejki już nie ma: żaden Twój obserwator jej nie obsługuje i żadne zadanie na niej nie czeka</p>`}
      ${read.error && !gone && html`<p class="role-problem">${refusalReason(read.error)}</p>`}
      ${view && !gone && html`
        <p class="muted">tryb: ${modeText(view.mode)}</p>
        <h3>Obserwatory (${view.observers.length})</h3>
        ${!view.observers.length && html`<p class="muted">brak — wpisy czekają na decyzję osoby</p>`}
        ${view.observers.map((observer) => html`<${ObserverCard} key=${observer.observer_id} store=${store} observer=${observer} onDialog=${onDialog} />`)}
        <h3>Wpisy (${view.entries.length})</h3>
        ${!view.entries.length && html`<p class="muted">brak zadań czekających na tej kolejce</p>`}
        ${view.entries.map((entry) => html`<${EntryCard} key=${entry.entry_id} entry=${entry} known=${known.has(entry.task_id)} onOpenTask=${onOpenTask} />`)}`}
    </div>
  </section>`;
}

export function ObserverCard({ store, observer, onDialog }) {
  const state = observerState(observer.state);
  const unapproved = unapprovedVersion(observer);
  const lastRun = lastRunText(observer.last_run);
  const commands = scheduleCommands(observer.state);
  const name = observer.name;
  const command = (kind) => store.mutate(
    (api) => (kind === 'pause' ? api.pauseObserver(name) : api.resumeObserver(name)),
    { success: `${name}: ${kind === 'pause' ? 'wstrzymany' : 'wznowiony'}`, failure: `${name}: odrzucone` },
  ).catch(() => {});
  return html`<article class=${`card observer-card${unapproved !== null ? ' unapproved' : ''}`} aria-label=${`Obserwator ${name}`}>
    <div class="observer-head">
      <strong>${name}</strong>
      <span class=${`role-${state.role}`}>${state.text}</span>
      ${unapproved !== null && html`<span class="chip role-attention" title="Harmonogram uruchamia wersję zatwierdzoną. Zatwierdź nową, aby ją uruchamiać.">! ${unapprovedText(observer)}</span>`}
    </div>
    <dl class="facts compact">
      ${versionRows(observer).map(([label, value]) => html`<dt>${label}</dt><dd class="mono">${value}</dd>`)}
      <dt>harmonogram</dt><dd>${scheduleText(observer)}</dd>
      <dt>tryb</dt><dd>${observer.mode}</dd>
      <dt>ostatni przebieg</dt><dd class=${observer.last_run?.error ? 'role-problem' : ''}>
        ${lastRun ?? 'jeszcze nie było'}${observer.last_run?.error ? ` — ${firstLine(observer.last_run.error, 160)}` : ''}</dd>
      ${observer.active_run !== undefined && observer.active_run !== null && html`<dt>trwa</dt><dd class="role-live">przebieg #${observer.active_run}</dd>`}
      ${observer.next_run_at && html`<dt>następny</dt><dd>${dateTime(observer.next_run_at)}</dd>`}
      ${observer.consecutive_failures > 0 && html`<dt>porażki z rzędu</dt><dd class="role-problem">${observer.consecutive_failures}</dd>`}
    </dl>
    ${observer.worker_missing && html`<p class="role-attention small-text">przebieg czeka na workera dłużej niż jeden interwał — czy narsor-observer-worker działa?</p>`}
    ${observer.note && html`<p class="role-attention small-text">${observer.note}</p>`}
    <${CliSettings} observer=${observer} />
    <div class="buttons wrap">
      ${unapproved !== null && html`<button class="small primary" onClick=${() => onDialog({ kind: 'approve', observer })}>Zatwierdź v${unapproved}…</button>`}
      <button class="small" onClick=${() => onDialog({ kind: 'script', observer })}>Skrypt…</button>
      <button class="small" onClick=${() => onDialog({ kind: 'edit', observer })}>Zmień…</button>
      ${commands.includes('pause') && html`<button class="small" onClick=${() => command('pause')}>Wstrzymaj</button>`}
      ${commands.includes('resume') && html`<button class="small" onClick=${() => command('resume')}>Wznów</button>`}
      <button class="small" onClick=${() => onDialog({ kind: 'delete', observer })}>Usuń…</button>
    </div>
  </article>`;
}

/** The settings the owner changes only in the CLI, read only (R34). */
function CliSettings({ observer }) {
  const secrets = observer.secrets ?? [];
  const mapping = Object.entries(observer.mapping ?? {});
  return html`<details class="cli-settings">
    <summary>ustawienia z CLI (tylko do odczytu)</summary>
    <dl class="facts compact">
      <dt>źródło</dt><dd>${observer.source}</dd>
      <dt>efekt</dt><dd>${observer.effect}${observer.workflow_id ? ` → ${observer.workflow_id}` : ''}</dd>
      <dt>limit zadań na przebieg</dt><dd>${observer.task_limit_per_run}</dd>
      <dt>sekrety</dt><dd class="mono">${secrets.length ? secrets.map((secret) => `${secret.name} → $${secret.env}`).join(', ') : 'brak'}</dd>
      <dt>mapowanie workflow</dt><dd class="mono">${mapping.length ? mapping.map(([input, field]) => `${input} ← ${field}`).join(', ') : 'brak'}</dd>
    </dl>
    <p class="hint">${CLI_HINT}</p>
  </details>`;
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
      ${(entry.observers ?? []).map((view) => html`<li key=${view.observer}>
        <strong>${view.observer}</strong>
        ${entryObserverParts(view).map((part) => html`<span class="muted"> · </span><span class=${`role-${part.role}`}>${part.text}</span>`)}
      </li>`)}
    </ul>
  </article>`;
}

// ---------------------------------------------------------------------------
// Forms and dialogs

/** The question before a dialog with an unsaved script closes. */
export const DISCARD_SCRIPT = 'Skrypt ma niezapisane zmiany. Zamknąć okno i odrzucić zmiany?';

/** Keys (`observer_id`, or `new` for the create form) of the dialogs with script text that is not saved. */
const unsavedScripts = new Set();

/** Whether a script editor or the create form has script text that is not saved. */
export function hasUnsavedScript() {
  return unsavedScripts.size > 0;
}

/** Whether the script text the owner typed (`null` before any typing) differs from the text the dialog opened with. */
export function scriptChanged(typed, opened) {
  return typed !== null && typed !== undefined && typed !== (opened ?? '');
}

/** Closes the dialog, after `ask` confirms when its script is `changed`; whether it closed. */
export function closeAfterAsking(changed, onClose, ask = (question) => globalThis.confirm(question)) {
  if (changed && !ask(DISCARD_SCRIPT)) return false;
  onClose();
  return true;
}

/** An open dialog of the page, over its overlay. */
const OPEN_DIALOG = '.overlay [role="dialog"]';

/**
 * Whether a key of the queues page (Escape, Q, ?, u) must not act: the key
 * comes from inside a dialog, a dialog is open (focus can sit on the page
 * body after a click on the overlay), or a script is not saved. Each of
 * them would leave the page and drop the dialog without the question.
 */
export function queuesKeysHeld(target, root = globalThis.document) {
  return hasUnsavedScript()
    || Boolean(target?.closest?.('[role="dialog"]'))
    || Boolean(root?.querySelector?.(OPEN_DIALOG));
}

/**
 * The dialog that gets focus back when the owner keeps editing: the dialog
 * around `element` (✕, Anuluj), the dialog in it (the overlay), or else the
 * open dialog of the page (Escape).
 */
export function dialogNear(element, root = globalThis.document) {
  return element?.closest?.('[role="dialog"]')
    ?? element?.querySelector?.('[role="dialog"]')
    ?? root?.querySelector?.(OPEN_DIALOG)
    ?? null;
}

/**
 * Gives focus back to `dialog` when the focus is not in it (after a click on
 * the overlay it is on the page body). A field that has the focus (the
 * script text after Escape) keeps it, with its caret.
 */
export function keepFocusIn(dialog, active = globalThis.document?.activeElement) {
  if (dialog && !dialog.contains(active)) dialog.focus();
}

/**
 * Lists the dialog `key` among the unsaved scripts while `changed` (for the
 * guard before the tab closes), and returns the close that asks first. The
 * overlay click, Escape, ✕ and Anuluj all go through it. When the owner
 * keeps editing, the focus stays in the dialog, so the next Escape goes to
 * the dialog (which asks again) and not to the page.
 */
function useScriptDraft(key, changed, onClose) {
  useEffect(() => {
    if (changed) unsavedScripts.add(key);
    else unsavedScripts.delete(key);
  }, [key, changed]);
  useEffect(() => () => unsavedScripts.delete(key), [key]);
  return (event) => {
    if (!closeAfterAsking(changed, onClose)) keepFocusIn(dialogNear(event?.currentTarget));
  };
}

/** The problem of a refused write, at the top of its form. */
function Refusal({ text }) {
  return text ? html`<p class="alert role-problem" role="alert">${text}</p>` : null;
}

/**
 * Create an observer (name, source, queue, mode, schedule, first script) or,
 * with `observer`, change its queue, mode and schedule. A refusal shows the
 * server's reason above the fields and keeps what the owner typed.
 */
function ObserverFormDialog({ store, observer = null, defaults = {}, onClose, onDone }) {
  const editing = observer !== null;
  const [form, setForm] = useState(() => observerForm(observer, defaults));
  const [problems, setProblems] = useState({});
  const [refusal, setRefusal] = useState(null);
  const [created, setCreated] = useState(null);
  const [sending, setSending] = useState(false);
  const set = (name) => (value) => setForm((old) => ({ ...old, [name]: value }));
  const close = useScriptDraft(editing ? `edit:${observer.observer_id}` : 'new', scriptChanged(form.script, ''), onClose);
  const field = (name, label, description, required = false) => html`<${Field} id=${`observer-${name}`} name=${label}
    kind=${{ type: 'text', options: [] }} required=${required} description=${description} problem=${problems[name]}
    value=${form[name]} onInput=${set(name)} />`;

  const submit = async (event) => {
    event.preventDefault();
    setRefusal(null);
    const { body, problems: found } = editing ? updateRequest(form, observer) : createRequest(form);
    setProblems(found);
    if (found.form) { setRefusal(found.form); return; }
    if (Object.keys(found).length) return;
    setSending(true);
    try {
      if (editing) {
        const result = await store.mutate((api) => api.updateObserver(observer.name, body), { success: `${observer.name}: zmieniony`, inline: true });
        if (result !== undefined) onDone(result.queue);
        return;
      }
      let made = created;
      if (!made) {
        made = await store.mutate((api) => api.createObserver(body), { success: `${body.name}: utworzony`, inline: true });
        if (made === undefined) return;
        setCreated(made);
      }
      if (form.script.trim()) {
        try {
          const saved = await store.mutate((api) => api.saveObserverScript(made.name, form.script), {
            success: (script) => `${made.name}: zapisano wersję ${script.version} — nie zatwierdzona`, inline: true,
          });
          if (saved === undefined) return;
        } catch (error) {
          setRefusal(`Obserwator ${made.name} utworzony, ale skrypt odrzucony: ${refusalReason(error)}`);
          return;
        }
      }
      onDone(made.queue);
    } catch (error) {
      setRefusal(refusalReason(error));
    } finally {
      setSending(false);
    }
  };

  return html`<${Dialog} title=${editing ? `Zmień obserwatora ${observer.name}` : 'Nowy obserwator'} onClose=${close} wide>
    <form class="answer-form observer-form" onSubmit=${submit} novalidate>
      <${Refusal} text=${refusal} />
      ${!editing && field('name', 'nazwa', 'unikalna, np. gitlab-mr', true)}
      ${!editing && field('source', 'źródło', 'co obserwuje, np. gitlab.example.com', true)}
      ${field('queue', 'kolejka', editing ? 'watch.queue bramek, które obsługuje' : 'watch.queue bramek; nazwa obserwatora, gdy puste', editing)}
      <${Field} id="observer-mode" name="tryb" kind=${{ type: 'select', options: OBSERVER_MODES }} required
        description="on_episodes: tylko dla wpisów kolejki; always: przy każdym uruchomieniu, bez wpisu. Wszystkie obserwatory kolejki mają ten sam tryb."
        problem=${problems.mode} value=${form.mode} onInput=${set('mode')} />
      <fieldset class="schedule">
        <legend>harmonogram</legend>
        <label class="toggle"><input type="radio" name="schedule" value="interval" checked=${form.schedule === 'interval'}
          onChange=${() => set('schedule')('interval')} /> interwał</label>
        <label class="toggle"><input type="radio" name="schedule" value="cron" checked=${form.schedule === 'cron'}
          onChange=${() => set('schedule')('cron')} /> cron</label>
        ${form.schedule === 'interval'
          ? field('interval', 'co ile sekund', 'co najmniej 60', true)
          : field('cron', 'wyrażenie cron', 'pięć pól (minuta godzina dzień miesiąc dzień-tygodnia), UTC, np. */10 * * * *', true)}
      </fieldset>
      ${!editing && html`<div class="field">
        <label for="observer-script">pierwszy skrypt <span class="muted">(może być pusty)</span></label>
        <textarea id="observer-script" class="script-text" rows="12" spellcheck="false" value=${form.script}
          onInput=${(event) => set('script')(event.currentTarget.value)}></textarea>
        <div class="field-help"><span class="muted">Zapis tworzy wersję 1 i jej nie zatwierdza.</span></div>
      </div>`}
      ${!editing && html`<p class="hint">${CLI_HINT}</p>`}
      <div class="buttons">
        <button type="submit" class="primary" disabled=${sending}>${editing ? 'Zapisz zmiany' : created ? 'Zapisz skrypt' : 'Utwórz'}</button>
        <button type="button" onClick=${close}>Anuluj</button>
      </div>
    </form>
  </${Dialog}>`;
}

/** Reads the text of script `version` of `observer`; `null` while there is none to read. */
function useScriptText(store, observer, version) {
  return useLoaded(
    () => (version === null || version === undefined ? Promise.resolve(null) : store.api.observerScript(observer.name, version)),
    `${observer.observer_id}:${version}`,
  );
}

/** The script editor: opens with the newest version; a save makes the next one, never approved (R32). */
function ScriptEditor({ store, observer, onClose, onDone }) {
  const latest = observer.latest_version ?? null;
  const read = useScriptText(store, observer, latest);
  const [text, setText] = useState(null);
  const [refusal, setRefusal] = useState(null);
  const [sending, setSending] = useState(false);
  const shown = text ?? read.data?.script ?? '';
  const waiting = latest !== null && read.loading && text === null;
  const close = useScriptDraft(observer.observer_id, scriptChanged(text, read.data?.script), onClose);

  const save = async (event) => {
    event.preventDefault();
    setRefusal(null);
    if (!shown.trim()) { setRefusal('Skrypt jest pusty.'); return; }
    if (latest !== null && read.data && shown === read.data.script) { setRefusal(`Bez zmian względem v${latest}.`); return; }
    setSending(true);
    try {
      const saved = await store.mutate((api) => api.saveObserverScript(observer.name, shown), {
        success: (script) => `${observer.name}: zapisano wersję ${script.version} — nie zatwierdzona`, inline: true,
      });
      if (saved !== undefined) onDone();
    } catch (error) {
      setRefusal(refusalReason(error));
    } finally {
      setSending(false);
    }
  };

  return html`<${Dialog} title=${`Skrypt ${observer.name}`} onClose=${close} wide>
    <form class="answer-form" onSubmit=${save}>
      <${Refusal} text=${refusal} />
      <p class="muted small-text">${latest === null ? 'Brak zapisanego skryptu.' : `Otwarta wersja v${latest}.`}
        Zapis tworzy wersję ${(latest ?? 0) + 1} i jej nie zatwierdza: harmonogram uruchamia dalej ${observer.approved_version ? `v${observer.approved_version}` : 'nic'}.</p>
      ${read.error && html`<p class="role-attention small-text">nie wczytano tekstu v${latest}: ${refusalReason(read.error)} — wklej cały skrypt</p>`}
      <textarea class="script-text" rows="20" spellcheck="false" aria-label="Tekst skryptu" disabled=${waiting}
        value=${waiting ? 'wczytuję…' : shown} onInput=${(event) => setText(event.currentTarget.value)}></textarea>
      <div class="buttons">
        <button type="submit" class="primary" disabled=${sending || waiting}>Zapisz nową wersję</button>
        <button type="button" onClick=${close}>Anuluj</button>
      </div>
    </form>
  </${Dialog}>`;
}

/** Approve the newest version: the dialog shows its text before the owner confirms (R32). */
function ApproveDialog({ store, observer, onClose, onDone }) {
  const version = unapprovedVersion(observer);
  const read = useScriptText(store, observer, version);
  const [refusal, setRefusal] = useState(null);
  const [sending, setSending] = useState(false);
  const script = read.data?.script ?? null;

  const approve = async () => {
    setRefusal(null);
    setSending(true);
    try {
      const result = await store.mutate((api) => api.approveObserver(observer.name, version), {
        success: `${observer.name}: zatwierdzono v${version}`, inline: true,
      });
      if (result !== undefined) onDone();
    } catch (error) {
      setRefusal(refusalReason(error));
    } finally {
      setSending(false);
    }
  };

  return html`<${Dialog} title=${`Zatwierdzić v${version} obserwatora ${observer.name}?`} onClose=${onClose} wide>
    <div class="confirm">
      <${Refusal} text=${refusal} />
      <p>Od teraz harmonogram uruchamia tę wersję${observer.approved_version ? ` zamiast v${observer.approved_version}` : ''}. Przeczytaj ją przed zatwierdzeniem.</p>
      ${read.loading && !script && html`<p class="muted">wczytuję tekst skryptu…</p>`}
      ${read.error && html`<p class="role-problem">nie wczytano tekstu skryptu: ${refusalReason(read.error)} — bez niego nie można zatwierdzić tutaj</p>`}
      ${script !== null && html`
        <p class="muted small-text">v${read.data.version} · zapisana ${dateTime(read.data.created_at)} · <span class="mono">${read.data.sha256}</span></p>
        <pre class="script-text" aria-label="Tekst skryptu">${script}</pre>`}
      <div class="buttons">
        <button class="primary" disabled=${script === null || sending} onClick=${approve}>Zatwierdź v${version}</button>
        <button onClick=${onClose}>Anuluj</button>
      </div>
    </div>
  </${Dialog}>`;
}

/** Delete names the observer: it removes its scripts, run errors and entries, for good (R33). */
function DeleteDialog({ store, observer, onClose, onDone }) {
  const [refusal, setRefusal] = useState(null);
  const [sending, setSending] = useState(false);
  const remove = async () => {
    setRefusal(null);
    setSending(true);
    try {
      const result = await store.mutate((api) => api.deleteObserver(observer.name), { success: `${observer.name}: usunięty`, inline: true });
      if (result !== undefined) onDone();
    } catch (error) {
      setRefusal(refusalReason(error));
    } finally {
      setSending(false);
    }
  };
  return html`<${Dialog} title=${`Usunąć obserwatora ${observer.name}?`} onClose=${onClose}>
    <div class="confirm">
      <${Refusal} text=${refusal} />
      <p>Usunięcie obserwatora <strong>${observer.name}</strong> usuwa jego skrypty i błędy przebiegów, a gdy to ostatni obserwator kolejki <span class="mono">${observer.queue}</span>, także jej wpisy. Tego nie da się cofnąć.</p>
      <p class="muted small-text">Zadania, które utworzył, zostają.</p>
      <div class="buttons">
        <button class="danger" disabled=${sending} onClick=${remove}>Usuń ${observer.name}</button>
        <button onClick=${onClose}>Anuluj</button>
      </div>
    </div>
  </${Dialog}>`;
}
