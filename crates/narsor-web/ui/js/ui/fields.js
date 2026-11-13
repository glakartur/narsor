// One editable field of a server form: text, multiline, select or integer,
// with its description and its problem under it.

import { html } from '../lib.js';

export function Field({ id, name, kind, required, description, value, problem, onInput }) {
  const common = {
    id,
    'aria-invalid': problem ? 'true' : undefined,
    'aria-describedby': `${id}-help`,
  };
  let input;
  if (kind.type === 'select') {
    input = html`<select ...${common} value=${value} onChange=${(event) => onInput(event.currentTarget.value)}>
      <option value="">— wybierz —</option>
      ${kind.options.map((option) => html`<option value=${option}>${option}</option>`)}
    </select>`;
  } else if (kind.type === 'multiline') {
    input = html`<textarea ...${common} rows="3" value=${value} onInput=${(event) => onInput(event.currentTarget.value)}></textarea>`;
  } else {
    input = html`<input ...${common} type="text" inputmode=${kind.type === 'integer' ? 'numeric' : undefined}
      value=${value} onInput=${(event) => onInput(event.currentTarget.value)} />`;
  }
  return html`<div class="field">
    <label for=${id}>${name}${required ? html` <span class="muted">(wymagane)</span>` : ''}</label>
    ${input}
    <div id=${`${id}-help`} class="field-help">
      ${problem ? html`<span class="role-problem">${problem}</span>` : description ? html`<span class="muted">${description}</span>` : ''}
    </div>
  </div>`;
}
