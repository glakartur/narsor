// The forms of the owner's panel, as the server describes them
// (`TaskSummary.panel`, `TaskSummary.actions`), and how a filled form
// becomes a request. The rules repeat `narsor_protocol::panel::deliver`
// (`crates/narsor-protocol/src/panel/mod.rs`) and the TUI's field checks
// (`src/tui/chooser.rs`). Pure functions, tested in Node.

export const MESSAGE_FIELD = 'Wiadomość dla agenta';
export const COMMENT_TO_AGENT = 'Komentarz dla agenta';
export const COMMENT_FIELD = 'komentarz';
export const EXTRA_VARIABLES_FIELD = 'variables (klucz=wartość)';
export const PATTERN_FIELD = 'Wzorzec polecenia';
const MAX_PATTERN_LEN = 500;

/** An externally tagged enum value as `{tag, value}`: `"answer"` or `{output: {...}}`. */
export function tagOf(tagged) {
  if (typeof tagged === 'string') return { tag: tagged, value: null };
  if (tagged && typeof tagged === 'object') {
    const [tag] = Object.keys(tagged);
    return { tag: tag ?? 'unknown', value: tagged[tag] ?? null };
  }
  return { tag: 'unknown', value: null };
}

/** How a panel field is edited: `{type: 'text'|'multiline'|'select'|'integer', options}`. */
export function fieldKind(field) {
  const { tag, value } = tagOf(field.kind);
  if (tag === 'select') return { type: 'select', options: value?.options ?? [] };
  if (tag === 'text') return { type: value?.multiline ? 'multiline' : 'text', options: [] };
  if (tag === 'integer') return { type: 'integer', options: [] };
  return { type: 'text', options: [] };
}

/** A proposed value as editable text: a string as itself, anything else as JSON. */
export function valueAsText(value) {
  return typeof value === 'string' ? value : JSON.stringify(value);
}

/** The text a field starts with. */
export function initialValue(field) {
  if (field.value !== undefined && field.value !== null) return String(field.value);
  if (field.proposed !== undefined && field.proposed !== null) return valueAsText(field.proposed);
  return '';
}

/** What is wrong with `text` in `field`, or `null`. */
export function fieldProblem(field, text, kind = fieldKind(field)) {
  if (text.trim() === '') return field.required ? `Pole '${field.name}' jest wymagane.` : null;
  if (kind.type === 'integer' && !/^\s*[-+]?\d+\s*$/.test(text)) return `Pole '${field.name}' wymaga liczby całkowitej.`;
  if (kind.type === 'select' && kind.options.length && !kind.options.includes(text)) {
    return `'${text}' nie jest dozwoloną wartością pola '${field.name}'.`;
  }
  return null;
}

/** The problems of the fields of `entry` (by field name) and the values to send (blank fields left out). */
export function checkEntry(entry, texts) {
  const problems = {};
  const values = [];
  for (const field of entry.fields ?? []) {
    const text = texts[field.name] ?? initialValue(field);
    const problem = fieldProblem(field, text);
    if (problem) problems[field.name] = problem;
    if (text.trim() !== '') values.push([field.name, text]);
  }
  return { problems, values };
}

const valueOf = (values, name) => values.find(([field]) => field === name)?.[1];

function insertOnce(map, name, key, value) {
  if (Object.hasOwn(map, key)) throw new Error(`Klucz '${name}.${key}' jest podany dwa razy.`);
  map[key] = value;
}

function deliveredValue(entry, name, text) {
  const proposed = (entry.fields ?? []).find((field) => field.name === name)?.proposed;
  return proposed !== undefined && proposed !== null && valueAsText(proposed) === text ? proposed : text;
}

function output(entry, label, values) {
  const artifacts = {};
  const variables = {};
  for (const [name, text] of values) {
    if (name.startsWith('artifacts.')) insertOnce(artifacts, 'artifacts', name.slice(10), valueAsText(deliveredValue(entry, name, text)));
    if (name.startsWith('variables.')) insertOnce(variables, 'variables', name.slice(10), deliveredValue(entry, name, text));
  }
  for (const line of (valueOf(values, EXTRA_VARIABLES_FIELD) ?? '').split('\n')) {
    if (!line.trim()) continue;
    const at = line.indexOf('=');
    if (at < 0) throw new Error(`Linia '${line}' w variables nie ma postaci klucz=wartość.`);
    const key = line.slice(0, at).trim();
    if (!key) throw new Error(`Linia '${line}' w variables nie ma klucza.`);
    insertOnce(variables, 'variables', key, line.slice(at + 1).trim());
  }
  const comment = valueOf(values, COMMENT_FIELD);
  return {
    chosen_label: label,
    artifacts,
    variables,
    ...(comment !== undefined ? { comment: comment.trim() } : {}),
  };
}

function pattern(decision, values) {
  if (decision.scope !== 'pattern') return undefined;
  const text = (valueOf(values, PATTERN_FIELD) ?? '').trim();
  // eslint-disable-next-line no-control-regex
  if (!text || text.length > MAX_PATTERN_LEN || /[()\u0000-\u001f\u007f]/.test(text)) {
    throw new Error('Wzorzec nie może być pusty ani zawierać nawiasów lub znaków sterujących.');
  }
  return text;
}

const asStrings = (values) => Object.fromEntries(values.map(([name, text]) => [name, text]));

/**
 * The `delivery` of `POST …/input` for entry `index` of `definition` with
 * `values` (pairs of field name and text). Throws `Error` with a Polish
 * message when the values cannot be sent.
 */
export function deliver(definition, index, values) {
  const entry = definition.entries?.[index];
  if (!entry) throw new Error('Ta akcja nie istnieje.');
  const { tag, value } = tagOf(entry.plan);
  let delivery;
  switch (tag) {
    case 'complete': {
      const map = asStrings(values);
      if (value?.label_field && value?.label) map[value.label_field] = value.label;
      delivery = { values: map, chosen_label: value?.label ?? null };
      break;
    }
    case 'answer':
      if (valueOf(values, 'answer') === undefined && valueOf(values, 'option') === undefined) {
        throw new Error('Wpisz odpowiedź albo wybierz opcję.');
      }
      delivery = { values: asStrings(values) };
      break;
    case 'output':
      delivery = output(entry, value.label, values);
      break;
    case 'message':
      delivery = { agent_message: valueOf(values, MESSAGE_FIELD) ?? '' };
      break;
    case 'approve':
      delivery = { action: 'approve' };
      break;
    case 'return':
      delivery = { comment: valueOf(values, COMMENT_TO_AGENT) ?? '', action: 'resume' };
      break;
    case 'permission': {
      const decision = { ...value.decision };
      const chosen = pattern(decision, values);
      if (chosen !== undefined) decision.pattern = chosen;
      const message = (valueOf(values, MESSAGE_FIELD) ?? '').trim();
      if (message) decision.message = message;
      else delete decision.message;
      delivery = { permission_decision: decision };
      break;
    }
    case 'entry_limit':
      delivery = { values: { entry_limit_decision: value.decision } };
      break;
    default:
      throw new Error('Ta odpowiedź wymaga nowszej wersji klienta.');
  }
  return { ...delivery, episode: definition.episode };
}

/** The forms of a task's panel and the one that opens first. */
export function panelForms(task) {
  const forms = task.panel?.forms ?? [];
  const index = task.panel?.default;
  return { forms, initial: Number.isInteger(index) && index < forms.length ? index : 0 };
}

const FORM_NAMES = {
  answer: 'Odpowiedź',
  permission: 'Uprawnienie',
  entry_limit: 'Limit wejść',
  chat: 'Wiadomość',
};

/** The name of a form in the switcher. */
export function formName(form) {
  if (form.kind === 'action') return form.waiting_id ? 'Wynik kroku' : 'Akcje';
  return FORM_NAMES[form.kind] ?? form.kind;
}

/** A key for the draft of a form: the task, the wait and the kind. */
export function formKey(taskId, form) {
  return `${taskId}/${form.waiting_id ?? '-'}/${form.kind}/${form.definition?.episode ?? ''}`;
}

// ---------------------------------------------------------------------------
// Actions (U8)

const PLACEHOLDER = /^\{\{(.+)\}\}$/;

/** `command` with every string `{{name}}` replaced by the trimmed field value (blank → `null`). */
export function fillPlaceholders(command, texts) {
  if (typeof command === 'string') {
    const match = PLACEHOLDER.exec(command);
    if (!match) return command;
    const text = (texts[match[1]] ?? '').trim();
    return text === '' ? null : text;
  }
  if (Array.isArray(command)) return command.map((item) => fillPlaceholders(item, texts));
  if (command && typeof command === 'object') {
    return Object.fromEntries(Object.entries(command).map(([key, value]) => [key, fillPlaceholders(value, texts)]));
  }
  return command;
}

/**
 * The request an action sends: `{route: 'owner'|'operator'|'comment', body}`.
 * Throws `Error` when it cannot be sent.
 */
export function resolveSend(send, texts) {
  switch (send?.type) {
    case 'owner':
      if (typeof send.command !== 'string') throw new Error('Ta akcja wymaga nowszej wersji klienta.');
      return { route: 'owner', body: send.command };
    case 'operator':
      if (!send.command || typeof send.command.kind !== 'string') throw new Error('Ta akcja wymaga nowszej wersji klienta.');
      return { route: 'operator', body: fillPlaceholders(send.command, texts) };
    case 'comment': {
      const text = (texts[send.field] ?? '').trim();
      if (!text) throw new Error(`Pole '${send.field}' jest wymagane.`);
      return { route: 'comment', body: text };
    }
    default:
      throw new Error('Ta akcja wymaga nowszej wersji klienta.');
  }
}

/** The actions of the panel, in order; `null` when the server sent none. */
export function panelActions(task) {
  if (!Array.isArray(task.actions)) return null;
  return task.actions
    .filter((action) => (action.surface ?? 'panel') === 'panel' && action.label)
    .sort((a, b) => (a.order ?? 0) - (b.order ?? 0));
}
