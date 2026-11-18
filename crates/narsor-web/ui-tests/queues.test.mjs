// Checks of the read-only queues view (U11): the texts in `text.js`, the
// queue mark in `model.js`, the routes and refusals in `store.js`, and what
// the queue list and the queue page show.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { queueEntryMark } from '../ui/js/model.js';
import { hashOf, refusalReason, routeOf } from '../ui/js/store.js';
import { ApiError, errorFrom } from '../ui/js/api.js';
import { entryAutomationParts, intervalText, scheduleText } from '../ui/js/text.js';
import { QueueDetail, QueueTable } from '../ui/js/ui/queues.js';

/** Every text of a vnode tree (with titles and labels), spaces collapsed. */
function textOf(node) {
  const walk = (item) => {
    if (item === null || item === undefined || typeof item === 'boolean') return '';
    if (typeof item === 'string' || typeof item === 'number') return String(item);
    if (Array.isArray(item)) return item.map(walk).join('');
    if (typeof item.type === 'function') return walk(item.type(item.props));
    const own = ['title', 'aria-label'].map((key) => item.props?.[key]).filter(Boolean).join(' ');
    return ` ${own} ${walk(item.props?.children)} `;
  };
  return walk(node).replace(/\s+/g, ' ');
}

/** Every element type of a vnode tree (`button`, `form`, …). */
function tagsOf(node) {
  const tags = [];
  const walk = (item) => {
    if (item === null || item === undefined || typeof item !== 'object') return;
    if (Array.isArray(item)) { item.forEach(walk); return; }
    if (typeof item.type === 'function') { walk(item.type(item.props)); return; }
    tags.push({ type: item.type, text: textOf(item) });
    walk(item.props?.children);
  };
  walk(node);
  return tags;
}

test('the queue page shows automations and entries with no controls that change them', () => {
  const view = {
    queue: 'mr-review', mode: 'on_episodes', interval_secs: 300,
    automations: [{
      automation: 'gitlab-mr', description: 'checks merge requests', scripts: ['in', 'check'],
      state: 'error', worker_missing: false, consecutive_failures: 3,
      last_run: { sequence: 4, status: 'failed', finished_at: '2026-11-17T10:00:00Z', error: 'boom' },
      errors: [{
        run_id: 'r4', sequence: 4, phase: 'check', script_digest: 'abc', status: 'failed',
        claimed_at: '2026-11-17T09:59:00Z', finished_at: '2026-11-17T10:00:00Z', exit_code: 2, error: 'boom', stderr: 'trace',
      }],
    }],
    entries: [{
      entry_id: 'e1', task_id: 'task-1', step: 'review', episode: 'ep', started_at: '2026-11-17T09:00:00Z',
      automations: [{ automation: 'gitlab-mr', running: true }],
    }],
    problems: [],
  };
  const tree = QueueDetail({ view, known: new Set(['task-1']) });
  const text = textOf(tree);
  assert.match(text, /gitlab-mr/);
  assert.match(text, /in, check/);
  assert.match(text, /porażki z rzędu 3/);
  assert.match(text, /#4 check · failed, kod 2/);
  assert.match(text, /zmieni się plik kolejki, automation.yaml lub skrypt check/);
  assert.match(text, /sprawdza teraz/);

  const tags = tagsOf(tree);
  assert.ok(!tags.some((tag) => ['form', 'textarea', 'input', 'select'].includes(tag.type)));
  const buttons = tags.filter((tag) => tag.type === 'button').map((tag) => tag.text.trim());
  assert.deepEqual(buttons, ['task-1 id:task-1'], 'only the link to the entry task');
});

test('a queue file that did not load is listed with its reason, next to the queues that did', () => {
  const tree = QueueTable({
    list: {
      queues: [
        { queue: 'mr-review', mode: 'on_episodes', interval_secs: 300, automations: ['gitlab-mr'], entries: 2 },
        { queue: 'jira', mode: 'always', cron: '0 9 * * 1', automations: [], entries: 0, problem: 'unknown automation x' },
      ],
      problems: [{ file: 'queues/broken.yaml', reason: 'expected a mapping' }],
    },
    onOpen: () => {},
  });
  const text = textOf(tree);
  assert.match(text, /mr-review on_episodes co 5m gitlab-mr 2/);
  assert.match(text, /problem: unknown automation x/);
  assert.match(text, /queues\/broken\.yaml · expected a mapping/);
  assert.match(text, /Pliki katalogu, których nie wczytano \(1\)/);
});

test('the schedule and per-entry texts', () => {
  assert.equal(scheduleText({ interval_secs: 300 }), 'co 5m');
  assert.equal(scheduleText({ interval_secs: 7200 }), 'co 2h');
  assert.equal(intervalText(90), 'co 90s');
  assert.equal(scheduleText({ cron: '*/10 * * * *' }), 'cron */10 * * * * (UTC)');
  assert.deepEqual(entryAutomationParts({ automation: 'x' }), [{ text: 'jeszcze nie sprawdzony', role: 'done' }]);
  const parts = entryAutomationParts({
    automation: 'x', running: true,
    last_error: { run_id: 'x.4', status: 'failed', finished_at: '2026-11-17T10:00:00Z', exit_code: 2, error: 'boom\nmore' },
  });
  assert.deepEqual(parts.map((part) => part.role), ['live', 'problem']);
  assert.equal(parts[1].text, 'błąd failed, kod 2: boom');
});

test('R21: one mark while the current episode has a queue entry', () => {
  assert.equal(queueEntryMark({ task_id: 't' }), null);
  const mark = queueEntryMark({ queue_entry: { queue: 'mr-review', step: 'review_work', episode: 'ep', since: 'x' } });
  assert.equal(mark.queue, 'mr-review');
  assert.equal(mark.text, 'kolejka mr-review · review_work');
});

test("a refused read shows the server's reason", () => {
  assert.equal(refusalReason(errorFrom(400, '{"code":"bad","message":"queue name is invalid"}')), 'queue name is invalid');
  assert.equal(refusalReason(errorFrom(409, '{"code":"stale"}')), 'stale');
  assert.match(refusalReason(new ApiError({ kind: 'unavailable' })), /brak połączenia/);
  assert.match(refusalReason(errorFrom(404, '')), /nie znaleziono/);
});

test('the queues routes round-trip through the address', () => {
  assert.deepEqual(routeOf('#/queues'), { page: 'queues', queue: null });
  assert.deepEqual(routeOf('#/queues/mr%20review'), { page: 'queues', queue: 'mr review' });
  assert.deepEqual(routeOf('#/task/task-1/events'), { page: 'tasks', taskId: 'task-1', tab: 'events' });
  assert.equal(routeOf('#token=abc'), null);
  assert.equal(hashOf({ page: 'queues', queue: 'mr review' }), '#/queues/mr%20review');
  assert.equal(hashOf({ page: 'queues', queue: null }), '#/queues');
  assert.equal(hashOf({ page: 'tasks', selectedTaskId: 'task-1', tab: 'info' }), '#/task/task-1/info');
});
