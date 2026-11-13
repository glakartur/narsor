// The rules that turn the server's task summaries into what the operator
// sees: the state icon, the counters, the ages, the queue text. They are the
// same rules as the TUI's (`src/tui/task_app/list.rs`, `detail.rs`), so the
// two interfaces always say the same thing about a task.
//
// Everything here is a pure function of its arguments. Nothing reads the
// clock or the page, so `ui-tests/` can check the rules with plain Node.

/** How many automatic resumes a lost task gets (`RESUME_ATTEMPT_LIMIT`). */
export const RESUME_ATTEMPT_LIMIT = 5;

/** The colour role of a piece of text; a CSS class `role-<name>` shows it. */
export const Role = Object.freeze({
  attention: 'attention',
  problem: 'problem',
  live: 'live',
  calm: 'calm',
  done: 'done',
});

/** Every state icon: its glyph, its word and its role. */
export const StateIcon = Object.freeze({
  archived: { key: 'archived', glyph: '▪', word: 'archived', role: Role.done },
  problem: { key: 'problem', glyph: '!', word: 'problem', role: Role.problem },
  needsReply: { key: 'needsReply', glyph: '?', word: 'awaiting input', role: Role.attention },
  blocked: { key: 'blocked', glyph: '⊘', word: 'blocked', role: Role.calm },
  done: { key: 'done', glyph: '✓', word: 'done', role: Role.done },
  pausing: { key: 'pausing', glyph: '◑', word: 'pausing', role: Role.calm },
  starting: { key: 'starting', glyph: '⟳', word: 'starting', role: Role.live },
  resuming: { key: 'resuming', glyph: '↺', word: 'resuming', role: Role.calm },
  stopped: { key: 'stopped', glyph: '‖', word: 'paused', role: Role.calm },
  queued: { key: 'queued', glyph: '◷', word: 'queued', role: Role.calm },
  running: { key: 'running', glyph: '●', word: 'running', role: Role.live },
  unknown: { key: 'unknown', glyph: '·', word: 'unknown', role: Role.done },
  sending: { key: 'sending', glyph: '↻', word: 'sending', role: Role.calm },
});

const PROBLEM_KINDS = new Set(['problem', 'failed_step', 'provisioning_failed']);
const QUESTION_KINDS = new Set(['question', 'approval', 'conductor_proposal']);

/**
 * What became of a task whose workload was lost: resumed automatically,
 * given up on, or lost while it was pausing. `null` when nothing was lost.
 */
export function lossState(task) {
  if (task.runner_state !== 'interrupted') return null;
  const resume = task.resume ?? {};
  if (resume.exhausted) return { kind: 'exhausted', reason: resume.exhausted.reason ?? '' };
  if (resume.next_attempt_at) {
    return {
      kind: 'auto_resume',
      attempt: Math.max(resume.consecutive_losses ?? 0, 1),
      limit: RESUME_ATTEMPT_LIMIT,
    };
  }
  if (resume.last_loss?.from === 'pause_requested') return { kind: 'lost_while_pausing' };
  return null;
}

/** Whether a loss leaves the task waiting for the operator. */
export function lossNeedsOwner(loss) {
  return loss !== null && loss.kind !== 'auto_resume';
}

/** `'problem'`, `'question'` or `null`: what the task wants from the operator. */
export function attentionOf(task) {
  const kinds = (task.attention ?? []).map((item) => item.kind);
  if (task.runner?.problem || lossNeedsOwner(lossState(task)) || kinds.some((kind) => PROBLEM_KINDS.has(kind))) {
    return 'problem';
  }
  return kinds.some((kind) => QUESTION_KINDS.has(kind)) ? 'question' : null;
}

/** Whether no live controller holds the task's backend. */
export function controllerOffline(task) {
  return !task.controller || task.controller.freshness !== 'live';
}

/** The state icon of a task. The first rule that holds wins. */
export function stateIcon(task) {
  const { state, runner_state: runnerState } = task;
  const attention = attentionOf(task);
  const loss = lossState(task);
  const autoResume = loss?.kind === 'auto_resume';
  const queueReason = task.queue?.reason?.reason;

  if (state === 'archived') return StateIcon.archived;
  if (attention === 'problem' || state === 'error') return StateIcon.problem;
  if (attention === 'question' || state === 'human_needed') return StateIcon.needsReply;
  const queueBlocked =
    state === 'queued' && (queueReason === 'controller_offline' || queueReason === 'backend_unknown');
  if (queueBlocked || ((state === 'running' || autoResume) && controllerOffline(task))) {
    return StateIcon.blocked;
  }
  if (state === 'finished') return StateIcon.done;
  if (state === 'paused' && runnerState === 'pause_requested') return StateIcon.pausing;
  if (runnerState === 'provisioning') return StateIcon.starting;
  if (autoResume) return StateIcon.resuming;
  if (state === 'paused') return StateIcon.stopped;
  if (state === 'queued') return StateIcon.queued;
  if (state === 'running') return StateIcon.running;
  return StateIcon.unknown;
}

/** Whether the icon is one of the loud ones: the task waits for the operator. */
export function needsOperator(icon) {
  return icon === StateIcon.needsReply || icon === StateIcon.problem;
}

/** The word of the status row: `failed` for a task that ended in an error. */
export function statusWord(task) {
  const icon = stateIcon(task);
  return icon === StateIcon.problem && task.state === 'error' ? 'failed' : icon.word;
}

/** How many tasks ask a question and how many have a problem. */
export function counters(tasks) {
  let questions = 0;
  let problems = 0;
  for (const task of tasks) {
    const icon = stateIcon(task);
    if (icon === StateIcon.needsReply) questions += 1;
    if (icon === StateIcon.problem) problems += 1;
  }
  return { questions, problems };
}

/** The ticket of a task, or `id:` and the start of its identifier. */
export function shortIdentifier(task) {
  const ticket = (task.ticket ?? '').trim();
  return ticket || `id:${shortId(task.task_id)}`;
}

/** The first characters of a task identifier, as every client shows it. */
export function shortId(taskId) {
  return String(taskId).slice(0, 11);
}

/** The title of a task, or its identifier when it has none. */
export function titleOf(task) {
  return (task.title ?? '').trim() || task.task_id;
}

/** Whether `task` matches the list filter (ticket, title or full id). */
export function matchesFilter(task, filter) {
  const needle = filter.trim().toLowerCase();
  if (!needle) return true;
  return [task.task_id, titleOf(task), task.ticket ?? ''].some((text) =>
    String(text).toLowerCase().includes(needle),
  );
}

/** Whether `z` hides the task: it is finished or archived. */
export function isDone(task) {
  return task.state === 'finished' || task.state === 'archived';
}

/**
 * The place of each queued task among the queued tasks of its backend, in
 * list order, counted from 1. A map from task id to place.
 */
export function queuePositions(tasks) {
  const next = new Map();
  const places = new Map();
  for (const task of tasks) {
    if (task.runner_state !== 'queued') continue;
    const place = (next.get(task.backend_id) ?? 0) + 1;
    next.set(task.backend_id, place);
    places.set(task.task_id, place);
  }
  return places;
}

/** When the activity the list shows the age of began; `null` for none. */
export function activitySince(task) {
  if (task.runner_state === 'provisioning') return task.state_since ?? null;
  switch (task.state) {
    case 'queued':
      return task.queue?.since ?? null;
    case 'human_needed': {
      const times = (task.attention ?? []).map((item) => item.since).sort();
      return times[0] ?? null;
    }
    case 'finished':
    case 'error':
    case 'archived':
      return null;
    default:
      return task.state_since ?? null;
  }
}

/** Seconds from `since` to `now`, never negative. */
export function secondsBetween(since, now) {
  return Math.max(0, Math.floor((new Date(now) - new Date(since)) / 1000));
}

/** An age in one unit: `40s`, `14m`, `2h`, `3d`. */
export function shortAge(seconds) {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

/** The age of an activity on the list: `<1m`, `14m`, `2h 5m`, `3d`. */
export function activityAge(seconds) {
  if (seconds < 60) return '<1m';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) {
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    return minutes === 0 ? `${hours}h` : `${hours}h ${minutes}m`;
  }
  return `${Math.floor(seconds / 86400)}d`;
}

/** The time to the reset of a usage limit: `45m`, `4h10m`, `2d3h`. */
export function timeToReset(now, resetsAt) {
  const minutes = Math.max(0, Math.floor((new Date(resetsAt) - new Date(now)) / 60000));
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days >= 1) return `${days}d${hours}h`;
  if (hours >= 1) return `${hours}h${minutes % 60}m`;
  return `${minutes}m`;
}

/** A utilization (0..1) as a whole percent between 0 and 100. */
export function percent(utilization) {
  const value = Math.round(Number(utilization) * 100);
  return Number.isNaN(value) ? 0 : Math.min(100, Math.max(0, value));
}

/** A token count in at most four characters: `304`, `1.5k`, `120k`, `2.4M`. */
export function scaled(count) {
  if (count < 1000) return String(count);
  const [divisor, unit] = count < 999_500 ? [1000, 'k'] : [1_000_000, 'M'];
  const quotient = count / divisor;
  if (quotient >= 10) return `${Math.round(quotient)}${unit}`;
  const tenths = Math.floor(quotient * 10 + 0.5) / 10;
  return `${Number.isInteger(tenths) ? tenths.toFixed(0) : tenths.toFixed(1)}${unit}`;
}

/** The icon of a step, by what runs in it and who decides. */
export function stepIcon(actionType, decisionBy) {
  switch (actionType) {
    case 'agent':
      return '✨';
    case 'script':
      return '💻';
    case 'profile':
      return '⚖️';
    case 'wait':
      return '✋';
    case 'decision':
      if (decisionBy === 'conductor') return '⚡🔀';
      if (decisionBy === 'human') return '🧑🔀';
      return '🔀';
    default:
      return '';
  }
}

/** The current step of a running task with its icon, or `''`. */
export function currentStep(task) {
  const cursor = task.runner?.execution_cursor;
  const name = (cursor?.current_state ?? '').trim();
  if (!name) return '';
  const icon = stepIcon(cursor.current_action_type, cursor.current_decision_by);
  return icon ? `${icon} ${name}` : name;
}

/** What the queue says about a queued task. */
export function queueText(task, place) {
  const prefix = place ? `#${place} · ` : '';
  const reason = task.queue?.reason;
  switch (reason?.reason) {
    case 'waiting_for_capacity':
      return `#${place ?? reason.position} · czeka na slot (${reason.running}/${reason.limit})`;
    case 'controller_offline':
      return `kontroler offline ${shortAge(secondsBetween(task.queue.since, task.as_of))}`;
    case 'admitted':
      return `${prefix}przyjęte, czeka na start`;
    case 'backend_unknown':
      return 'backend nieznany';
    default:
      return `${prefix}w kolejce`;
  }
}

/** The slots of the backend of `selected` (or of the first task). */
export function slots(tasks, selected) {
  const backend = (selected ?? tasks[0])?.backend_id;
  if (backend === undefined) return null;
  const own = tasks.filter((task) => task.backend_id === backend);
  for (const task of own) {
    const reason = task.queue?.reason;
    if (reason?.reason === 'waiting_for_capacity') return { used: reason.running, limit: reason.limit };
  }
  return { used: own.filter((task) => task.holds_slot).length, limit: null };
}

/** The line under the header for a lost task or an unavailable harness. */
export function notice(task) {
  const loss = lossState(task);
  if (loss) {
    const role = lossNeedsOwner(loss) ? Role.problem : Role.attention;
    return { role, text: describeLoss(loss) };
  }
  const unavailable = task.runner?.default_harness_unavailable;
  if (unavailable) {
    return { role: Role.attention, text: `${unavailable.harness} niedostępny tutaj — zostaje ${unavailable.kept}` };
  }
  return null;
}

function describeLoss(loss) {
  switch (loss.kind) {
    case 'auto_resume':
      return `wznowienie automatyczne, próba ${loss.attempt}/${loss.limit}`;
    case 'exhausted':
      return `nie udało się wznowić automatycznie: ${exhaustionDetail(loss.reason)}, uruchom od nowa (Akcje)`;
    default:
      return 'utracone podczas wstrzymywania, czeka na Ciebie (Akcje)';
  }
}

/** The reason an automatic resume gave up, without the server's prefix. */
export function exhaustionDetail(reason) {
  const detail = String(reason).replace(/^could not resume automatically: /, '');
  return detail === 'the workload was lost' ? 'runner utracony' : detail;
}

/** `'archived'`, `'not_started'` or `'started'`: whether a task has details to read. */
export function readiness(task) {
  if (task.state === 'archived') return 'archived';
  if (!task.runner && task.state === 'queued') return 'not_started';
  return 'started';
}

/** The pull request of a workspace: its label and, when safe, its address. */
export function pullRequest(workspace) {
  const url = safeLinkUrl(workspace.pr_url);
  let number = String(workspace.pr ?? '').trim().replace(/^#/, '');
  if (!number && workspace.pr_url) {
    const last = String(workspace.pr_url).split('/').filter(Boolean).pop() ?? '';
    if (/^\d+$/.test(last)) number = last;
  }
  if (!number && !url) return null;
  return { label: number ? `#${number}` : 'PR', url };
}

/** `url` when it is an absolute http(s) address with a host; else `null`. */
export function safeLinkUrl(url) {
  if (typeof url !== 'string' || /[\s\u0000-\u001f\u007f]/.test(url)) return null;
  try {
    const parsed = new URL(url);
    return (parsed.protocol === 'http:' || parsed.protocol === 'https:') && parsed.host ? url : null;
  } catch {
    return null;
  }
}

/** The branches of a workspace: `branch → base`, or the one that exists. */
export function branchText(workspace) {
  const { branch, base_branch: base } = workspace;
  if (branch && base && branch !== base) return `${branch} → ${base}`;
  return branch || base || '';
}

/** The Polish word for `count` tasks. */
export function tasksWord(count) {
  if (count === 1) return 'zadanie';
  const lastTwo = count % 100;
  const last = count % 10;
  return last >= 2 && last <= 4 && !(lastTwo >= 12 && lastTwo <= 14) ? 'zadania' : 'zadań';
}
