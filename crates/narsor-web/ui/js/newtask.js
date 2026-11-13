// The new-task form: the fields of a workflow and the request they make.
// Rules from `src/tui/task_app/new_task/mod.rs` and `src/tui/input_form.rs`:
// ticket (15 characters) and title (50) first, then each workspace as a
// repository and its branch, then the inputs. Pure functions, tested in Node.

export const TICKET_FIELD = 'Ticket';
export const TITLE_FIELD = 'Tytuł';
export const TICKET_MAX = 15;
export const TITLE_MAX = 50;
const INPUT_LABELS = { description: 'Opis' };

export const repositoryField = (workspace) => `workspace:${workspace}:repository`;
export const branchField = (workspace) => `workspace:${workspace}:branch`;

const asText = (value) => (typeof value === 'string' ? value : JSON.stringify(value));

function inputDescription(input) {
  const kind = { integer: 'liczba całkowita', boolean: 'true / false' }[input.type];
  const said = (input.description ?? '').trim();
  return [kind, said].filter(Boolean).join(' · ') || null;
}

/** The branch a workspace starts from: its base branch, else the repository's default. */
export function suggestedBranch(workspace, repositories, repositoryId) {
  const base = (workspace.base_branch ?? '').trim();
  if (base) return base;
  return (repositories.find((repo) => repo.id === repositoryId)?.default_branch ?? '').trim();
}

/**
 * The fields of a workflow's form, in screen order. Each field:
 * `{name, label, type, options, required, description, initial, max, workspace?, role?}`.
 */
export function formFields(workflow, repositories) {
  const fields = [
    { name: TICKET_FIELD, label: TICKET_FIELD, type: 'text', max: TICKET_MAX, description: 'numer zadania w Jirze, opcjonalnie', initial: '' },
    { name: TITLE_FIELD, label: TITLE_FIELD, type: 'text', max: TITLE_MAX, description: 'krótki tytuł; bez niego nada go dyrygent, opcjonalnie', initial: '' },
  ];
  for (const workspace of workflow.workspaces ?? []) {
    const ids = repositories.map((repo) => repo.id);
    const firstRepo = ids[0] ?? '';
    fields.push({
      name: repositoryField(workspace.name),
      label: `${workspace.name} · repozytorium`,
      type: ids.length ? 'select' : 'text',
      options: ids,
      required: true,
      description: workspace.description ?? null,
      initial: firstRepo,
      workspace: workspace.name,
      role: 'repository',
    });
    fields.push({
      name: branchField(workspace.name),
      label: `${workspace.name} · gałąź`,
      type: 'branch',
      initial: suggestedBranch(workspace, repositories, firstRepo),
      workspace: workspace.name,
      role: 'branch',
    });
  }
  for (const input of workflow.inputs ?? []) {
    const type = { boolean: 'select', integer: 'integer', text: 'multiline' }[input.type] ?? 'text';
    const hasDefault = input.default !== undefined && input.default !== null;
    fields.push({
      name: input.name,
      label: INPUT_LABELS[input.name] ?? input.name,
      type,
      options: input.type === 'boolean' ? ['true', 'false'] : [],
      required: Boolean(input.required) && !hasDefault,
      description: inputDescription(input),
      initial: hasDefault ? asText(input.default) : input.type === 'boolean' ? 'false' : '',
      input,
    });
  }
  return fields;
}

/** What is wrong with a field, or `null`. */
export function problemOf(field, text) {
  const value = text ?? '';
  if (!value.trim()) return field.required ? `Pole '${field.label}' jest wymagane.` : null;
  if (field.max && [...value].length > field.max) return `Pole '${field.label}' ma najwyżej ${field.max} znaków.`;
  if (field.type === 'integer' && !/^\s*[-+]?\d+\s*$/.test(value)) return `Pole '${field.label}' wymaga liczby całkowitej.`;
  if (field.type === 'select' && field.options.length && !field.options.includes(value)) {
    return `'${value}' nie jest dozwoloną wartością pola '${field.label}'.`;
  }
  return null;
}

/** The typed inputs: every declared input with a value or a default. */
export function inputValues(inputs, texts) {
  const typed = {};
  for (const input of inputs ?? []) {
    const raw = (texts[input.name] ?? '').trim();
    if (!raw) {
      if (input.default !== undefined && input.default !== null) typed[input.name] = input.default;
      continue;
    }
    if (input.type === 'integer') typed[input.name] = Number.parseInt(raw, 10);
    else if (input.type === 'boolean') typed[input.name] = raw.toLowerCase() === 'true';
    else typed[input.name] = raw;
  }
  return typed;
}

/**
 * Checks the form. Returns `{problems}` (by field name) or `{launch}`, the
 * `launch` of `POST /control/v1/tasks`.
 */
export function buildLaunch(workflow, fields, texts) {
  const problems = {};
  for (const field of fields) {
    const problem = problemOf(field, texts[field.name] ?? field.initial);
    if (problem) problems[field.name] = problem;
  }
  if (Object.keys(problems).length) return { problems };
  const value = (name) => (texts[name] ?? fields.find((field) => field.name === name)?.initial ?? '').trim();
  const workspaces = {};
  for (const workspace of workflow.workspaces ?? []) {
    const branch = value(branchField(workspace.name));
    workspaces[workspace.name] = { repository: value(repositoryField(workspace.name)), branch: branch || null, commit: null };
  }
  const launch = {
    workflow_id: workflow.name,
    workflow_version: workflow.version,
    inputs: inputValues(workflow.inputs, Object.fromEntries(fields.map((field) => [field.name, texts[field.name] ?? field.initial]))),
  };
  if (Object.keys(workspaces).length) launch.workspaces = workspaces;
  if (value(TICKET_FIELD)) launch.ticket = value(TICKET_FIELD);
  if (value(TITLE_FIELD)) launch.title = value(TITLE_FIELD);
  return { launch };
}
