// *Przebieg*: the axis of the timeline positions. Choosing a step shows its
// conversation. The current step is followed until the operator chooses
// another one.

import { html, useEffect, useState } from '../../lib.js';
import { scaled, stepIcon } from '../../model.js';
import { clock, duration, positionGlyph, positionRole } from '../../text.js';
import { Conversation } from './conversation.js';

function tokens(usage) {
  const total = usage?.total;
  return total ? `↑${scaled(total.input ?? 0)} ↓${scaled(total.output ?? 0)}` : '';
}

function Step({ position, selected, now, onSelect }) {
  const end = position.ended_at ? new Date(position.ended_at) : now;
  const took = position.started_at ? duration(end - new Date(position.started_at)) : '';
  const usage = position.usage;
  const second = [position.harness, position.model, position.effort].filter(Boolean).join(' · ');
  return html`
    <li class=${`step${selected ? ' selected' : ''}`}>
      <button class="step-button" aria-pressed=${selected} onClick=${() => onSelect(position.label)}>
        <span class=${`step-glyph role-${positionRole(position.status)}`} aria-label=${position.status}>${positionGlyph(position.status)}</span>
        <span class="step-body">
          <span class="step-line">
            <span class="step-name">${stepIcon(position.action_type, position.decision_by)} ${position.label}</span>
            <span class="muted">${clock(position.started_at)}${took ? ` · ${took}` : ''}</span>
          </span>
          <span class="step-line muted small-text">
            ${second}
            ${typeof usage?.cost_usd === 'number' ? ` · $${usage.cost_usd.toFixed(2)}` : ''}
            ${tokens(usage) ? ` · ${tokens(usage)}` : ''}
            ${usage?.subagent_count ? ` · ◈ ${usage.subagent_count}` : ''}
          </span>
          ${position.error && html`<span class="step-line role-problem small-text">${position.error}</span>`}
        </span>
      </button>
    </li>`;
}

export function Progress({ api, task, timeline, now }) {
  const positions = timeline?.positions ?? [];
  const current = timeline?.default_position ?? positions[positions.length - 1]?.label ?? null;
  const [chosen, setChosen] = useState(null);
  useEffect(() => setChosen(null), [task.task_id]);
  const selected = chosen ?? current;
  const position = positions.find((p) => p.label === selected) ?? null;
  if (!positions.length) return html`<p class="empty">brak kroków</p>`;
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
        ${position && html`<${Conversation} api=${api} task=${task} positions=${[position]}
          emptyText=${position.conversation?.state === 'not_created' ? `rozmowa nie powstała: ${position.conversation.reason ?? ''}` : 'ten krok nie ma rozmowy'} />`}
      </div>
    </div>`;
}
