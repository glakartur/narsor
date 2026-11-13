// *Info*: the sealed inputs, the workflow, the workspace, the runner and the
// controller of the task (labels from `src/tui/task_app/tabs/info.rs`).

import { html } from '../../lib.js';
import { secondsBetween, shortAge } from '../../model.js';

const STATE_NAME = {
  queued: 'w kolejce', running: 'działa', human_needed: 'czeka na Ciebie', paused: 'wstrzymane',
  finished: 'zakończone', error: 'błąd', archived: 'zarchiwizowane',
};

function ago(task, at) {
  return at ? `${shortAge(secondsBetween(at, task.as_of))} temu` : '';
}

function slotText(task) {
  if (task.holds_slot) return 'zajęty';
  if (task.queue && task.state === 'queued' && task.resume?.next_attempt_at) return 'w kolejce po wznowieniu — dostanie slot jako pierwsze';
  if (task.state === 'human_needed' || task.state === 'paused') return 'bez slotu — odpowiedź lub wznowienie przywraca zadanie';
  return 'brak';
}

function queueText(queue) {
  const reason = queue?.reason;
  switch (reason?.reason) {
    case 'waiting_for_capacity': return `czeka na slot: miejsce ${reason.position}, zajęte ${reason.running} z ${reason.limit}`;
    case 'controller_offline': return 'kontroler offline — nic nie startuje';
    case 'admitted': return 'przyjęte, czeka na start przez kontroler';
    case 'backend_unknown': return `backend nieznany: ${reason.detail ?? ''}`;
    default: return queue?.message ?? '';
  }
}

function Sheet({ title, rows }) {
  const shown = rows.filter(([, value]) => value !== null && value !== undefined && value !== '');
  return html`<section class="sheet">
    <h3>${title}</h3>
    <dl class="facts">${shown.map(([label, value, role]) => html`<dt>${label}</dt><dd class=${role ? `role-${role}` : ''}>${value}</dd>`)}</dl>
  </section>`;
}

export function Info({ task }) {
  const runner = task.runner;
  const observation = task.observation;
  const controller = task.controller;
  const answers = (task.inputs ?? []).map((input) => [`odpowiedź`, `${input.status} · ${ago(task, input.updated_at)}${input.detail ? ` · ${input.detail}` : ''}`]);
  return html`<div class="info">
    <${Sheet} title="Zapieczętowane wejścia" rows=${[
      ['zadanie', task.task_id],
      ['backend', task.backend_id],
      ['utworzone', `${new Date(task.created_at).toLocaleString('pl-PL')} (${ago(task, task.created_at)})`],
      ['launch digest', task.launch_digest],
      ['ticket', task.ticket],
      ['tytuł', task.title],
      ...(answers.length ? answers : [['odpowiedzi', 'brak', 'done']]),
    ]} />
    <${Sheet} title="Workflow" rows=${[
      ['workflow', `${task.workflow_id} v${task.workflow_version}`],
      ['stan', STATE_NAME[task.state] ?? task.state],
      ['slot', slotText(task)],
      ['pozycja', task.state === 'queued' ? String(task.rank + 1) : null],
      ['kolejka', task.queue ? queueText(task.queue) : null],
      ['archiwum', task.archive ? (task.archive.completed_at ? `zarchiwizowane ${ago(task, task.archive.completed_at)}` : `archiwizacja zlecona ${ago(task, task.archive.requested_at)}`) : null, 'done'],
    ]} />
    <${Sheet} title="Workspace" rows=${observation ? [
      ['workload', observation.summary?.workload_present ? 'jest' : 'brak'],
      ['storage', observation.summary?.storage_present ? 'jest' : 'brak'],
      ['obserwacja', `${observation.observation?.kind ?? ''} · ${observation.freshness === 'live' ? 'aktualne' : 'nieaktualne'} · ${ago(task, observation.observed_at)}`],
      ['szczegół', observation.summary?.detail, 'done'],
    ] : [['obserwacja', 'jeszcze nic', 'done']]} />
    <${Sheet} title="Runner" rows=${runner ? [
      ['status', runner.execution_status],
      ['krok', runner.execution_cursor?.current_state],
      ['incarnacja', runner.incarnation, 'done'],
      ['zgłoszenie', `${runner.freshness === 'live' ? 'aktualne' : 'nieaktualne'} · ${ago(task, runner.observed_at)}`],
      ['problem', runner.problem ? `${runner.problem.code}: ${runner.problem.message}` : null, 'problem'],
      ['dyrygent', runner.conductor_available ? 'dostępny' : `niedostępny: ${runner.conductor_unavailable_reason ?? ''}`],
      ['harness', runner.default_harness_unavailable ? `${runner.default_harness_unavailable.harness} niedostępny — zostaje ${runner.default_harness_unavailable.kept}` : null, 'attention'],
    ] : [['status', 'runner jeszcze się nie zgłosił', 'done']]} />
    <${Sheet} title="Kontroler" rows=${controller ? [
      ['kontroler', controller.controller_id],
      ['stan', controller.freshness === 'live' ? 'online' : 'offline', controller.freshness === 'live' ? 'live' : 'attention'],
      ['heartbeat', controller.last_heartbeat_at ? ago(task, controller.last_heartbeat_at) : 'nigdy', 'done'],
      ['epoka dzierżawy', String(controller.lease_epoch), 'done'],
    ] : [['kontroler', 'żaden nie trzyma dzierżawy backendu', 'attention']]} />
  </div>`;
}
