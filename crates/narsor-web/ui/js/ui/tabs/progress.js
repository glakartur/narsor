// *Przebieg*: the axis of the timeline positions. Choosing a step shows its
// conversation. The current step is followed until the operator chooses
// another one. In the history (`readOnly`) only the step rows are shown:
// the conversation is gone with the storage.

import { html, useEffect, useState } from '../../lib.js';
import { stepIcon } from '../../model.js';
import { clock, duration, gateLines, gateOverlay, gatePositionLabel, positionGlyph, positionRole, stepDetails } from '../../text.js';
import { Conversation } from './conversation.js';

function Step({ position, selected, now, onSelect }) {
  const end = position.ended_at ? new Date(position.ended_at) : now;
  const took = position.started_at ? duration(end - new Date(position.started_at)) : '';
  const body = html`
    <span class=${`step-glyph role-${positionRole(position.status)}`} aria-label=${position.status}>${positionGlyph(position.status)}</span>
    <span class="step-body">
      <span class="step-line">
        <span class="step-name">${stepIcon(position.action_type, position.decision_by)} ${position.label}</span>
        <span class="muted">${clock(position.started_at)}${took ? ` · ${took}` : ''}</span>
      </span>
      <span class="step-line muted small-text">${stepDetails(position)}</span>
      ${position.error && html`<span class="step-line role-problem small-text">${position.error}</span>`}
    </span>`;
  if (!onSelect) return html`<li class="step"><div class="step-button">${body}</div></li>`;
  return html`
    <li class=${`step${selected ? ' selected' : ''}`}>
      <button class="step-button" aria-pressed=${selected} onClick=${() => onSelect(position.label)}>${body}</button>
    </li>`;
}

/** The entry and exit scripts of the gate under its step (R20). */
export function GateScripts({ gate }) {
  return html`<ul class="gate-scripts small-text" aria-label="Skrypty kolejki">
    ${gateLines(gate).map((line) => html`<li class=${`role-${line.role}`}>${line.text}</li>`)}
  </ul>`;
}

export function Progress({ api, task, timeline, now, readOnly = false }) {
  const gate = task.gate_automation ?? null;
  const positions = gateOverlay(timeline?.positions, gate);
  const current = timeline?.default_position ?? positions[positions.length - 1]?.label ?? null;
  const [chosen, setChosen] = useState(null);
  useEffect(() => setChosen(null), [task.task_id]);
  const selected = chosen ?? current;
  const position = positions.find((p) => p.label === selected) ?? null;
  if (!positions.length) return html`<p class="empty">brak kroków</p>`;
  if (readOnly) {
    return html`<ol class="axis read-only" aria-label="Kroki">
      ${positions.map((p) => html`<${Step} key=${p.label} position=${p} now=${now} />`)}
    </ol>`;
  }
  return html`
    <div class="progress">
      <ol class="axis" aria-label="Kroki">
        ${positions.map((p) => html`<${Step} key=${p.label} position=${p} now=${now} selected=${p.label === selected}
          onSelect=${(label) => setChosen(label === current ? null : label)} />`)}
      </ol>
      <div class="step-detail">
        <div class="step-detail-head">
          <strong>${position?.label}</strong>
          <span class=${chosen ? 'muted' : 'role-live'}>${chosen ? html`· ręcznie <button class="link" onClick=${() => setChosen(null)}>podążaj</button>` : '↧ podąża'}</span>
        </div>
        ${gate && position && position.label === gatePositionLabel(positions, gate) && html`<${GateScripts} gate=${gate} />`}
        ${position && html`<${Conversation} api=${api} task=${task} positions=${[position]}
          emptyText=${position.conversation?.state === 'not_created' ? `rozmowa nie powstała: ${position.conversation.reason ?? ''}` : 'ten krok nie ma rozmowy'} />`}
      </div>
    </div>`;
}
