// The new-task dialog: a workflow from the catalogue (or a file on the
// server's disk, when the server reads them), its fields, and the request.
// The task id is made before the first attempt, so a retry after a lost
// answer sends the same request under the same id and the server never makes
// two tasks. The draft outlives closing the dialog.

import { html, useEffect, useRef, useState } from '../lib.js';
import { newTaskId } from '../api.js';
import { buildLaunch, formFields, repositoryField } from '../newtask.js';
import { Field } from './fields.js';

const PATH = '\u0000path';
const draft = { workflow: null, texts: {}, path: '' };

function useCatalogue(api) {
  const [state, setState] = useState({ loading: true });
  useEffect(() => {
    let live = true;
    Promise.all([
      api.workflows(),
      api.repositories().catch(() => ({ repositories: [] })),
      api.capabilities().catch(() => null),
    ]).then(
      ([workflows, repositories, capabilities]) => live && setState({
        workflows: workflows.workflows ?? [],
        repositories: repositories.repositories ?? [],
        capabilities,
      }),
      (error) => live && setState({ error: error.message }),
    );
    return () => { live = false; };
  }, [api]);
  return state;
}

function BranchField({ api, id, field, repository, value, problem, onInput }) {
  const [branches, setBranches] = useState({ list: [], note: null });
  useEffect(() => {
    if (!repository) return undefined;
    let live = true;
    setBranches({ list: [], note: 'wczytuję gałęzie…' });
    api.branches(repository).then(
      (answer) => live && setBranches({ list: answer.branches ?? [], note: null }),
      (error) => live && setBranches({ list: [], note: `gałęzie niedostępne: ${error.message}` }),
    );
    return () => { live = false; };
  }, [repository]);
  return html`<div class="field">
    <label for=${id}>${field.label}</label>
    <input id=${id} type="text" list=${`${id}-list`} value=${value} autocomplete="off" onInput=${(event) => onInput(event.currentTarget.value)} />
    <datalist id=${`${id}-list`}>${branches.list.map((branch) => html`<option value=${branch} />`)}</datalist>
    <div class="field-help">${problem ? html`<span class="role-problem">${problem}</span>` : html`<span class="muted">${branches.note ?? 'można wpisać inną gałąź'}</span>`}</div>
  </div>`;
}

export function NewTask({ store, onClose, onCreated }) {
  const api = store.api;
  const catalogue = useCatalogue(api);
  const [chosen, setChosen] = useState(draft.workflow);
  const [texts, setTexts] = useState(draft.texts);
  const [path, setPath] = useState(draft.path);
  const [checked, setChecked] = useState(null);
  const [problems, setProblems] = useState({});
  const [error, setError] = useState(null);
  const [sending, setSending] = useState(null);
  const ref = useRef(null);
  useEffect(() => { ref.current?.focus(); }, []);
  useEffect(() => { Object.assign(draft, { workflow: chosen, texts, path }); }, [chosen, texts, path]);

  const workflows = catalogue.workflows ?? [];
  const repositories = catalogue.repositories ?? [];
  const pathsAllowed = catalogue.capabilities?.workflow_paths;
  const current = chosen ?? workflows.find((workflow) => workflow.valid)?.name ?? null;
  const workflow = current === PATH ? checked : workflows.find((item) => item.name === current) ?? null;
  const fields = workflow?.valid ? formFields(workflow, repositories) : [];

  const setText = (name, text) => {
    const field = fields.find((item) => item.name === name);
    setTexts((old) => {
      const next = { ...old, [name]: text };
      // Another repository: its branch starts again from its default.
      if (field?.role === 'repository') {
        const branch = fields.find((item) => item.role === 'branch' && item.workspace === field.workspace);
        const workspace = workflow.workspaces.find((item) => item.name === field.workspace);
        next[branch.name] = (workspace.base_branch ?? '').trim()
          || (repositories.find((repo) => repo.id === text)?.default_branch ?? '');
      }
      return next;
    });
    setProblems((old) => ({ ...old, [name]: null }));
  };

  const check = async () => {
    setError(null);
    setChecked(null);
    try {
      const result = await api.validateWorkflow({ path: path.trim() });
      setChecked({ ...result, name: result.name ?? path.trim() });
    } catch (problem) {
      setError(problem.message);
    }
  };

  const submit = async (attempt) => {
    setError(null);
    const built = attempt?.request ? null : buildLaunch(workflow, fields, texts);
    if (built?.problems) { setProblems(built.problems); return; }
    const taskId = attempt?.taskId ?? newTaskId();
    const request = attempt?.request ?? (current === PATH
      ? { kind: 'path', body: {
        task_id: taskId, workflow: path.trim(), inputs: built.launch.inputs,
        ...(built.launch.workspaces ? { workspaces: Object.fromEntries(Object.entries(built.launch.workspaces).map(([name, ws]) => [name, { repository: ws.repository, branch: ws.branch }])) } : {}),
        ...(built.launch.ticket ? { ticket: built.launch.ticket } : {}),
        ...(built.launch.title ? { title: built.launch.title } : {}),
      } }
      : { kind: 'catalogue', body: { task_id: taskId, backend_id: null, launch: built.launch } });
    setSending({ taskId, request, state: 'sending' });
    try {
      await store.mutate(
        (client) => (request.kind === 'path' ? client.startTaskFromPath(request.body) : client.createTask(request.body)),
        { success: 'zadanie utworzone — trafi na koniec kolejki', failure: 'nie utworzono zadania' },
      ).then((result) => {
        if (result === undefined) { setSending(null); return; }
        Object.assign(draft, { workflow: null, texts: {}, path: '' });
        onCreated(taskId);
      });
    } catch (problem) {
      if (problem?.transient) setSending({ taskId, request, state: 'lost' });
      else { setSending(null); setError(problem?.message ?? String(problem)); }
    }
  };

  let form;
  if (current === PATH) {
    form = html`<div class="field">
        <label for="nt-path">Ścieżka do pliku workflow na dysku serwera</label>
        <div class="row"><input id="nt-path" type="text" value=${path} placeholder="/home/…/flow.yaml"
          onInput=${(event) => { setPath(event.currentTarget.value); setChecked(null); }} />
          <button type="button" onClick=${check} disabled=${!path.trim()}>Sprawdź</button></div>
      </div>
      ${checked && !checked.valid && html`<ul class="violations">${(checked.violations ?? []).map((v) => html`<li class="role-problem">${v}</li>`)}</ul>`}`;
  }
  if (workflow && !workflow.valid && current !== PATH) {
    form = html`<p class="role-problem">Tego workflow nie da się wybrać:</p>
      <ul class="violations">${(workflow.violations ?? []).map((v) => html`<li>${v}</li>`)}</ul>`;
  } else if (workflow?.valid) {
    form = html`${form ?? ''}
      ${workflow.description && html`<p class="muted">${workflow.description}</p>`}
      ${fields.map((field, i) => (field.role === 'branch'
        ? html`<${BranchField} api=${api} id=${`nt-${i}`} field=${field}
            repository=${(texts[repositoryField(field.workspace)] ?? fields.find((f) => f.name === repositoryField(field.workspace)).initial).trim()}
            value=${texts[field.name] ?? field.initial} problem=${problems[field.name]} onInput=${(text) => setText(field.name, text)} />`
        : html`<${Field} id=${`nt-${i}`} name=${field.label} kind=${{ type: field.type, options: field.options ?? [] }}
            required=${field.required} description=${field.description} problem=${problems[field.name]}
            value=${texts[field.name] ?? field.initial} onInput=${(text) => setText(field.name, text)} />`))}`;
  }

  return html`
    <div class="overlay" onClick=${onClose}>
      <div class="dialog newtask-dialog" role="dialog" aria-modal="true" aria-labelledby="nt-title" tabindex="-1" ref=${ref}
        onClick=${(event) => event.stopPropagation()}
        onKeyDown=${(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }}>
        <div class="dialog-head">
          <h2 id="nt-title">Nowe zadanie <span class="muted small-text">trafi na koniec kolejki</span></h2>
          <button class="icon" onClick=${onClose} aria-label="Zamknij">✕</button>
        </div>
        ${catalogue.loading && html`<p class="muted">wczytuję workflow…</p>`}
        ${catalogue.error && html`<p class="role-problem">${catalogue.error}</p>`}
        ${!catalogue.loading && !catalogue.error && html`
          <div class="newtask">
            <ul class="workflow-list" role="listbox" aria-label="Workflow">
              ${workflows.map((item) => html`<li>
                <button role="option" aria-selected=${item.name === current} class=${`workflow-item${item.name === current ? ' chosen' : ''}${item.valid ? '' : ' invalid'}`}
                  onClick=${() => { setChosen(item.name); setProblems({}); setError(null); }}>
                  <span class="action-label">${item.name}${item.valid ? '' : html` <span class="role-problem">✗</span>`}</span>
                  <span class="muted small-text">${item.origin_label ?? item.source}${item.version ? ` · v${item.version}` : ''}</span>
                </button></li>`)}
              <li><button role="option" aria-selected=${current === PATH} disabled=${!pathsAllowed}
                title=${pathsAllowed ? '' : catalogue.capabilities?.workflow_paths_reason ?? 'ten serwer nie czyta plików workflow ze swojego dysku'}
                class=${`workflow-item${current === PATH ? ' chosen' : ''}`} onClick=${() => setChosen(PATH)}>
                <span class="action-label">ścieżka…</span>
                <span class="muted small-text">${pathsAllowed ? 'plik na dysku serwera' : 'niedostępne na tym serwerze'}</span>
              </button></li>
            </ul>
            <form class="newtask-form" onSubmit=${(event) => { event.preventDefault(); submit(sending?.state === 'lost' ? sending : null); }}>
              ${form ?? html`<p class="muted">Wybierz workflow.</p>`}
              ${error && html`<p class="role-problem" role="alert">${error}</p>`}
              ${sending?.state === 'lost' && html`<p class="role-problem">nie wysłano — odpowiedź serwera nie dotarła. Ponowienie wyśle to samo zadanie (${sending.taskId}).</p>`}
              <div class="buttons">
                <button type="submit" class="primary" disabled=${!workflow?.valid || sending?.state === 'sending'}>
                  ${sending?.state === 'sending' ? 'wysyłanie…' : sending?.state === 'lost' ? 'Ponów' : 'Utwórz zadanie'}
                </button>
                <button type="button" onClick=${onClose}>Zamknij</button>
              </div>
            </form>
          </div>`}
      </div>
    </div>`;
}
