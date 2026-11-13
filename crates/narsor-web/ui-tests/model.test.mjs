// Checks of the display rules in `ui/js/model.js`. Run: `node --test crates/narsor-web/ui-tests/`.
import assert from 'node:assert/strict';
import { test } from 'node:test';

import * as model from '../ui/js/model.js';

const NOW = '2026-11-13T12:00:00Z';

function task(fields = {}) {
  return {
    task_id: 'task-0123456789abcdef',
    backend_id: 'local',
    runner_state: 'running',
    state: 'running',
    controller: { freshness: 'live' },
    attention: [],
    resume: {},
    as_of: NOW,
    ...fields,
  };
}

test('the state icon follows the precedence of the TUI', () => {
  const icon = (fields) => model.stateIcon(task(fields)).key;

  assert.equal(icon({ state: 'archived', attention: [{ kind: 'problem' }] }), 'archived');
  assert.equal(icon({ attention: [{ kind: 'question' }, { kind: 'failed_step' }] }), 'problem');
  assert.equal(icon({ state: 'error' }), 'problem');
  assert.equal(icon({ runner: { problem: 'x' } }), 'problem');
  assert.equal(icon({ attention: [{ kind: 'approval' }] }), 'needsReply');
  assert.equal(icon({ state: 'human_needed' }), 'needsReply');
  assert.equal(icon({ attention: [{ kind: 'entry_limit' }] }), 'running');
  assert.equal(icon({ controller: null }), 'blocked');
  assert.equal(icon({ state: 'queued', queue: { reason: { reason: 'controller_offline' } } }), 'blocked');
  assert.equal(icon({ state: 'finished', controller: null }), 'done');
  assert.equal(icon({ state: 'paused', runner_state: 'pause_requested' }), 'pausing');
  assert.equal(icon({ state: 'queued', runner_state: 'provisioning' }), 'starting');
  assert.equal(icon({ state: 'paused' }), 'stopped');
  assert.equal(icon({ state: 'queued', runner_state: 'queued' }), 'queued');
  assert.equal(icon({}), 'running');
  assert.equal(icon({ state: 'something_new' }), 'unknown');
});

test('a lost task is resumed, given up on, or lost while pausing', () => {
  const lost = (resume) => task({ state: 'paused', runner_state: 'interrupted', resume });

  assert.equal(model.stateIcon(lost({ next_attempt_at: NOW, consecutive_losses: 2 })).key, 'resuming');
  assert.deepEqual(model.lossState(lost({ next_attempt_at: NOW })), { kind: 'auto_resume', attempt: 1, limit: 5 });
  assert.equal(model.stateIcon(lost({ exhausted: { reason: 'r' } })).key, 'problem');
  assert.equal(model.stateIcon(lost({ last_loss: { from: 'pause_requested' } })).key, 'problem');
  assert.equal(model.stateIcon(lost({})).key, 'stopped');
  assert.equal(model.lossState(task({ resume: { exhausted: { reason: 'r' } } })), null);
});

test('the status word says failed only for a task that ended in an error', () => {
  assert.equal(model.statusWord(task({ state: 'error' })), 'failed');
  assert.equal(model.statusWord(task({ attention: [{ kind: 'problem' }] })), 'problem');
  assert.equal(model.statusWord(task({ state: 'paused' })), 'paused');
});

test('the counters count a task once and a problem wins over a question', () => {
  const tasks = [
    task({ attention: [{ kind: 'question' }] }),
    task({ attention: [{ kind: 'question' }, { kind: 'problem' }] }),
    task({}),
  ];

  assert.deepEqual(model.counters(tasks), { questions: 1, problems: 1 });
});

test('ages use the formats of the TUI', () => {
  assert.deepEqual([0, 59, 60, 3599, 3600, 86400].map(model.shortAge), ['0s', '59s', '1m', '59m', '1h', '1d']);
  assert.deepEqual([59, 60, 3600, 3660, 90000].map(model.activityAge), ['<1m', '1m', '1h', '1h 1m', '1d']);
  assert.equal(model.secondsBetween('2026-11-13T12:00:10Z', NOW), 0);
});

test('the time to a reset is in whole minutes, rounded down', () => {
  const at = (minutes) => new Date(new Date(NOW).getTime() + minutes * 60000 + 59000).toISOString();

  assert.deepEqual(
    [45, 60, 250, 1440, 3060, -5].map((minutes) => model.timeToReset(NOW, at(minutes))),
    ['45m', '1h0m', '4h10m', '1d0h', '2d3h', '0m'],
  );
});

test('token counts are scaled like the TUI scales them', () => {
  assert.deepEqual(
    [304, 1000, 1500, 1960, 72000, 120400, 999499, 999500, 2400000].map(model.scaled),
    ['304', '1k', '1.5k', '2k', '72k', '120k', '999k', '1M', '2.4M'],
  );
  assert.deepEqual([0.014, 0.9, 1.4, NaN].map(model.percent), [1, 90, 100, 0]);
});

test('the activity of a task starts where its state says', () => {
  const since = (fields) => model.activitySince(task(fields));

  assert.equal(since({ runner_state: 'provisioning', state: 'queued', state_since: 'a' }), 'a');
  assert.equal(since({ state: 'queued', queue: { since: 'q' }, state_since: 'a' }), 'q');
  assert.equal(since({ state: 'human_needed', attention: [{ since: '2' }, { since: '1' }] }), '1');
  assert.equal(since({ state: 'finished', state_since: 'a' }), null);
  assert.equal(since({ state: 'paused', state_since: 'a' }), 'a');
});

test('queue places are counted per backend among the queued tasks', () => {
  const queued = (id, backend) => task({ task_id: id, backend_id: backend, runner_state: 'queued', state: 'queued' });
  const places = model.queuePositions([queued('a', 'x'), task({ task_id: 'r' }), queued('b', 'y'), queued('c', 'x')]);

  assert.deepEqual([...places], [['a', 1], ['b', 1], ['c', 2]]);
});

test('the queue text names the reason', () => {
  const queued = (reason) => task({ state: 'queued', queue: { reason, since: '2026-11-13T11:59:20Z' } });

  assert.equal(
    model.queueText(queued({ reason: 'waiting_for_capacity', position: 4, running: 2, limit: 3 }), 2),
    '#2 · czeka na slot (2/3)',
  );
  assert.equal(model.queueText(queued({ reason: 'controller_offline' }), 1), 'kontroler offline 40s');
  assert.equal(model.queueText(queued({ reason: 'admitted' }), 1), '#1 · przyjęte, czeka na start');
  assert.equal(model.queueText(task({ state: 'queued' }), undefined), 'w kolejce');
});

test('the slots come from a waiting task, else from the slots held', () => {
  const waiting = task({ queue: { reason: { reason: 'waiting_for_capacity', running: 2, limit: 3 } } });

  assert.deepEqual(model.slots([task({ holds_slot: true }), waiting], null), { used: 2, limit: 3 });
  assert.deepEqual(model.slots([task({ holds_slot: true }), task({})], null), { used: 1, limit: null });
  assert.equal(model.slots([], null), null);
});

test('the filter looks in the ticket, the title and the full id', () => {
  const one = task({ ticket: 'NAR-12', title: 'Zmiana API' });

  assert.ok(model.matchesFilter(one, ' nar-1 '));
  assert.ok(model.matchesFilter(one, 'api'));
  assert.ok(model.matchesFilter(one, 'cdef'));
  assert.ok(!model.matchesFilter(one, 'brak'));
  assert.ok(model.matchesFilter(one, ''));
});

test('a pull request link is offered only for a safe address', () => {
  assert.deepEqual(model.pullRequest({ pr: '#42', pr_url: 'https://git.example/pr/42' }), {
    label: '#42',
    url: 'https://git.example/pr/42',
  });
  assert.deepEqual(model.pullRequest({ pr_url: 'https://git.example/pr/7' }), {
    label: '#7',
    url: 'https://git.example/pr/7',
  });
  assert.deepEqual(model.pullRequest({ pr: '5', pr_url: 'javascript:alert(1)' }), { label: '#5', url: null });
  assert.equal(model.pullRequest({}), null);
  assert.equal(model.safeLinkUrl('https://a b'), null);
});

test('the branches, the step and the notice read like the TUI', () => {
  assert.equal(model.branchText({ branch: 'feat', base_branch: 'main' }), 'feat → main');
  assert.equal(model.branchText({ branch: 'main', base_branch: 'main' }), 'main');
  assert.equal(model.branchText({ base_branch: 'main' }), 'main');
  assert.equal(
    model.currentStep(task({ runner: { execution_cursor: { current_state: 'review', current_action_type: 'profile' } } })),
    '⚖️ review',
  );
  assert.equal(model.stepIcon('decision', 'conductor'), '⚡🔀');
  assert.equal(model.exhaustionDetail('could not resume automatically: the workload was lost'), 'runner utracony');
  assert.equal(
    model.notice(task({ runner: { default_harness_unavailable: { harness: 'opencode', kept: 'claude-code' } } })).text,
    'opencode niedostępny tutaj — zostaje claude-code',
  );
});

test('the Polish plural of tasks follows the number', () => {
  assert.deepEqual([1, 2, 5, 12, 22, 25].map(model.tasksWord), [
    'zadanie', 'zadania', 'zadań', 'zadań', 'zadania', 'zadań',
  ]);
});
