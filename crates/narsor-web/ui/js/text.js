// Texts the views show that are not about one task's state: harness names,
// usage limits, the harness of the header. Pure functions, tested in Node.

import { percent, scaled, timeToReset } from './model.js';

const HARNESS_SHORT = {
  'claude-code': 'claude',
  codex: 'codex',
  opencode: 'opencode',
  'github-copilot': 'copilot',
};

/** The short name of a harness id; an unknown id as it is. */
export function harnessShort(id) {
  return HARNESS_SHORT[id] ?? id;
}

/**
 * The usage limits of the header: one group per harness, each with its
 * limits as `{text, role}`. Example text: `5h 1% ↻4h10m`.
 */
export function usageGroups(usage, now) {
  return (usage?.harnesses ?? []).map((harness) => ({
    name: harnessShort(harness.harness),
    limits: (harness.limits ?? []).map((limit) => ({
      text: `${limit.window} ${percent(limit.utilization)}% ↻${timeToReset(now, limit.resets_at)}`,
      role: limit.status === 'allowed_warning' ? 'attention' : limit.status === 'rejected' ? 'problem' : null,
    })),
  }));
}

/** The harness text of the header, as in the TUI. */
export function harnessText(snapshot, pending) {
  if (pending) return `harness ${pending}…`;
  if (!snapshot) return 'harness ?';
  return snapshot.override ? `harness ${snapshot.effective}` : `harness ${snapshot.effective} (konfiguracja)`;
}

// ---------------------------------------------------------------------------
// Task details (U6)


/** The first line of a text, trimmed, cut to `limit` characters with `…`. */
export function firstLine(text, limit = 80) {
  return elide(String(text ?? '').split('\n')[0], limit);
}

export function elide(text, limit) {
  const chars = [...String(text ?? '').trim()];
  return chars.length <= limit ? chars.join('') : `${chars.slice(0, limit).join('')}…`;
}

/** The first 8 characters of an id, or `unknown`. */
export function shortAttempt(id) {
  return id ? String(id).slice(0, 8) : 'unknown';
}

function shownValue(value) {
  return firstLine(typeof value === 'string' ? value : JSON.stringify(value));
}

/** The one-line detail of a workflow event, as in `src/tui/event_text.rs`. */
export function eventDetail(data) {
  const d = data ?? {};
  switch (d.type) {
    case 'workflow_started': return `workflow_started  ${d.workflow_id}`;
    case 'workflow_paused': return `workflow_paused  ${d.reason}`;
    case 'workflow_resumed': return 'workflow_resumed';
    case 'workflow_completed': return 'workflow_completed';
    case 'workflow_failed': return `workflow_failed  ${d.error}`;
    case 'workflow_cancelled': return `workflow_cancelled  ${d.reason}`;
    case 'state_entered': return `state_entered  ${d.state_id}`;
    case 'state_exited': return `state_exited  ${d.state_id} → ${d.output_label}`;
    case 'action_started': return `action_started  ${d.state_id} (${d.action_type})`;
    case 'action_completed': return `action_completed  ${d.output_label}`;
    case 'action_failed': return `action_failed  ${d.error}`;
    case 'action_attempt_failed': return `action_attempt_failed  ${d.error} (retrying in ${d.retry_in_ms}ms)`;
    case 'action_retrying': return `action_retrying  attempt ${d.retry_count}`;
    case 'action_output_rejected':
      return `action_output_rejected  attempt ${shortAttempt(d.action_execution_id)}: ${firstLine(d.reason)}`;
    case 'output_reprompted':
      return `output_reprompted  by ${d.by} (attempt ${shortAttempt(d.action_execution_id)}, after ${shortAttempt(d.previous_attempt)})`;
    case 'decision_requested': return `decision_requested  ${d.provider}`;
    case 'decision_made': return `decision_made  ${d.provider} → ${d.output_label}`;
    case 'human_input_requested': return `human_input_requested  ${d.instruction}`;
    case 'human_input_received': {
      const by = d.answered_by ?? 'operator';
      if (d.close_reason && d.close_reason !== 'answered') return `human_input_received  closed by ${d.close_reason} (${by})`;
      return d.output_label ? `human_input_received  ${by} → ${d.output_label}` : `human_input_received  ${by}`;
    }
    case 'problem_raised': return `problem_raised  ${d.kind} in ${d.state_id}: ${firstLine(d.reason)}`;
    case 'conductor_started': return `conductor_started  ${d.purpose} (${d.trigger})`;
    case 'conductor_finished':
      return d.error ? `conductor_finished  ${d.outcome}: ${firstLine(d.error)}` : `conductor_finished  ${d.outcome}`;
    case 'conductor_decision': {
      const confidence = d.confidence ?? 'low';
      return d.output_label
        ? `conductor_decision  ${d.kind} → ${d.output_label} (${confidence})`
        : `conductor_decision  ${d.kind} (${confidence})`;
    }
    case 'conductor_decision_undone': return `conductor_decision_undone  ${d.decision_id} → ${d.state_id}`;
    case 'human_transition_selected': return `human_transition_selected  ${d.from_state} → ${d.to_state}`;
    case 'variable_set_by_workflow':
      return `variable_set  ${d.variable} = ${shownValue(d.value)} (${d.state_id}, ${d.output_label})`;
    case 'transition_taken': return `transition_taken  ${d.from_state} → ${d.to_state} (${d.output_label})`;
    case 'entry_limit_reached': return `entry_limit_reached  ${d.state_id} (${d.entries}/${d.max_entries})`;
    case 'entry_limit_decision': return `entry_limit_decision  ${d.state_id} → ${d.decision}`;
    case 'entry_limit_counter_reset': return `entry_limit_reset  ${d.state_id} (${d.previous_entries})`;
    case 'artifact_created': return `artifact_created  ${d.artifact_id}`;
    case 'external_event_received': return `external_event_received  ${d.event_type}`;
    case 'error': return `error  ${d.message}`;
    case 'agent_session_started':
      return d.resumed_session
        ? `agent_session_started  ${d.harness} (attempt ${shortAttempt(d.action_execution_id)}, resuming ${shortAttempt(d.resumed_session)})`
        : `agent_session_started  ${d.harness} (attempt ${shortAttempt(d.action_execution_id)})`;
    case 'agent_session_completed':
      return `agent_session_completed  attempt ${shortAttempt(d.action_execution_id)} (session ${shortAttempt(d.harness_session_id)})`;
    case 'agent_session_failed':
      return `agent_session_failed  ${d.error} (attempt ${shortAttempt(d.action_execution_id)})`;
    case 'permission_denied_ignored': {
      const source = d.source ? `subagent ${elide(d.source.name, 40)}` : 'agent';
      return ['odmowa pominięta', source, elide(d.tool_name, 40), elide(d.summary, 80)].filter(Boolean).join(' · ');
    }
    case 'agent_message':
      return `agent_message  (${d.role}) attempt ${shortAttempt(d.action_execution_id)}: ${firstLine(d.content)}`;
    default:
      return 'unknown_event';
  }
}

function addCounts(sum, counts) {
  if (!counts) return sum;
  const base = sum ?? { input: 0, output: 0 };
  return { input: base.input + (counts.input ?? 0), output: base.output + (counts.output ?? 0) };
}

function addCost(sum, cost) {
  return typeof cost === 'number' ? (sum ?? 0) + cost : sum;
}

/** The usage of all positions of a timeline, summed; `null` when none reported. */
export function sumUsage(positions) {
  let sum = null;
  for (const position of positions ?? []) {
    const usage = position.usage;
    if (!usage) continue;
    sum ??= { total: null, subagents: null, count: 0, cost: null, conductor: null, conductorCost: null };
    sum.total = addCounts(sum.total, usage.total);
    sum.subagents = addCounts(sum.subagents, usage.subagents);
    sum.count += usage.subagent_count ?? 0;
    sum.cost = addCost(sum.cost, usage.cost_usd);
    sum.conductor = addCounts(sum.conductor, usage.conductor_total);
    sum.conductorCost = addCost(sum.conductorCost, usage.conductor_cost_usd);
  }
  return sum;
}

const money = (value) => `$${value.toFixed(2)}`;
const tokens = (counts) => `↑${scaled(counts.input)} ↓${scaled(counts.output)}`;
const group = (parts) => parts.filter(Boolean).join(' ');

/** The task usage row: `razem $… · ✨ $… ↑… ↓… (◈ n …) · ⚡ $… ↑… ↓…`; `null` when nothing. */
export function usageLine(sum) {
  if (!sum) return null;
  const parts = [];
  if (sum.cost !== null || sum.conductorCost !== null) parts.push(`razem ${money((sum.cost ?? 0) + (sum.conductorCost ?? 0))}`);
  const subs = sum.subagents && sum.subagents.input + sum.subagents.output > 0
    ? `(${group(['◈', sum.count > 0 ? String(sum.count) : null, tokens(sum.subagents)])})`
    : null;
  if (sum.cost !== null || sum.total) {
    parts.push(group(['✨', sum.cost !== null ? money(sum.cost) : null, sum.total ? tokens(sum.total) : null, subs]));
  }
  if (sum.conductorCost !== null || sum.conductor) {
    parts.push(group(['⚡', sum.conductorCost !== null ? money(sum.conductorCost) : null, sum.conductor ? tokens(sum.conductor) : null]));
  }
  return parts.length ? parts.join(' · ') : null;
}

/** Whether a conversation read stops after `page`, asked `from`. */
export function pagingDone(page, from) {
  if (page?.status !== 'recorded') return true;
  return !page.entries?.length || page.closed || page.truncated_tail || page.next_entry <= from;
}

/**
 * The entries of a conversation as items to draw: an entry of the main
 * agent stays one item; consecutive entries of one subagent become one
 * group `{subagent, entries, errors}`.
 */
export function groupEntries(entries) {
  const items = [];
  for (const entry of entries) {
    const sub = entry.subagent;
    if (!sub) {
      items.push({ entry });
      continue;
    }
    const last = items[items.length - 1];
    const errors = (entry.elements ?? []).filter((element) => element.kind === 'tool_result' && element.status === 'error').length;
    if (last?.subagent?.id === sub.id) {
      last.entries.push(entry);
      last.errors += errors;
    } else {
      items.push({ subagent: sub, entries: [entry], errors });
    }
  }
  return items;
}

/** The attempts of a position whose conversation is recorded. */
export function attemptsOf(position) {
  if (position?.conversation?.state !== 'recorded') return [];
  if (position.attempts?.length) return position.attempts;
  return position.action_execution_id ? [position.action_execution_id] : [];
}

/** The status glyph of a timeline position. */
export function positionGlyph(status) {
  return { succeeded: '✓', failed: '✗', interrupted: '✗', in_progress: '●', awaiting_decision: '?' }[status] ?? '·';
}

/** The role of a timeline position status. */
export function positionRole(status) {
  return { succeeded: 'done', failed: 'problem', interrupted: 'problem', in_progress: 'live', awaiting_decision: 'attention' }[status] ?? 'done';
}

/** A duration in one or two units: `45s`, `3m 05s`, `1h 02m`. */
export function duration(ms) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${String(seconds % 60).padStart(2, '0')}s`;
  return `${Math.floor(minutes / 60)}h ${String(minutes % 60).padStart(2, '0')}m`;
}

/** A clock time `HH:MM:SS` in the browser's zone. */
export function clock(at) {
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? '' : date.toLocaleTimeString('pl-PL', { hour12: false });
}
