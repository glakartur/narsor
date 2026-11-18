// Checks of the queues view (U11): the rules in `model.js`, the texts in
// `text.js`, the routes and refusals in `store.js`, and the observer card.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  createRequest, observerForm, queueEntryMark, scheduleCommands, unapprovedVersion, updateRequest,
} from '../ui/js/model.js';
import { Store, hashOf, refusalReason, routeOf } from '../ui/js/store.js';
import { ApiError, errorFrom } from '../ui/js/api.js';
import {
  entryObserverParts, intervalText, modeText, scheduleText, unapprovedText, versionRows,
} from '../ui/js/text.js';
import {
  CLI_HINT, DISCARD_SCRIPT, ObserverCard, closeAfterAsking, dialogNear, hasUnsavedScript, keepFocusIn, queuesKeysHeld, scriptChanged,
} from '../ui/js/ui/queues.js';

function observer(fields = {}) {
  return {
    observer_id: 'obs-1',
    name: 'gitlab-mr',
    state: 'active',
    effect: 'none',
    queue: 'mr-review',
    mode: 'on_episodes',
    interval_secs: 300,
    task_limit_per_run: 10,
    approved_version: 2,
    latest_version: 2,
    worker_missing: false,
    consecutive_failures: 0,
    created_at: '2026-11-17T10:00:00Z',
    ...fields,
  };
}

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

test('AE10: the marker shows while the newest version is not the approved one', () => {
  assert.equal(unapprovedVersion(observer({ approved_version: 2, latest_version: 3 })), 3);
  assert.equal(unapprovedText(observer({ approved_version: 2, latest_version: 3 })), 'wersja 3 nie zatwierdzona');
  assert.equal(unapprovedVersion(observer({ approved_version: undefined, latest_version: 1 })), 1);
  assert.equal(unapprovedVersion(observer({ approved_version: 3, latest_version: 3 })), null);
  assert.equal(unapprovedVersion(observer({ approved_version: undefined, latest_version: undefined })), null);
  assert.equal(unapprovedText(observer()), null);
});

test('the versions are two labelled rows (KTD-10)', () => {
  assert.deepEqual(versionRows(observer({ approved_version: 2, latest_version: 3 })), [
    ['wersja zatwierdzona', 'v2'],
    ['wersja najnowsza', 'v3'],
  ]);
  assert.deepEqual(versionRows(observer({ approved_version: undefined, latest_version: undefined })), [
    ['wersja zatwierdzona', 'brak'],
    ['wersja najnowsza', 'brak skryptu'],
  ]);
});

test('the observer card marks an unapproved version and offers its approval', () => {
  const store = { mutate: () => Promise.resolve() };
  const marked = textOf(ObserverCard({ store, observer: observer({ latest_version: 3 }), onDialog: () => {} }));
  assert.match(marked, /wersja 3 nie zatwierdzona/);
  assert.match(marked, /Zatwierdź v3…/);
  assert.match(marked, /wersja zatwierdzona v2/);
  assert.match(marked, /wersja najnowsza v3/);

  const plain = textOf(ObserverCard({ store, observer: observer(), onDialog: () => {} }));
  assert.doesNotMatch(plain, /nie zatwierdzona/);
  assert.doesNotMatch(plain, /Zatwierdź/);
});

test('the card shows the CLI settings read only, with the hint (R34)', () => {
  const store = { mutate: () => Promise.resolve() };
  const text = textOf(ObserverCard({
    store,
    observer: observer({
      effect: 'create_task', workflow_id: 'wruw-change', task_limit_per_run: 5,
      secrets: [{ name: 'gitlab-token', env: 'GITLAB_TOKEN' }], mapping: { ticket: 'key' },
    }),
    onDialog: () => {},
  }));
  assert.match(text, /create_task → wruw-change/);
  assert.match(text, /gitlab-token → \$GITLAB_TOKEN/);
  assert.match(text, /ticket ← key/);
  assert.ok(text.includes(CLI_HINT));
});

test('the schedule, mode and per-entry texts', () => {
  assert.equal(scheduleText(observer({ interval_secs: 300 })), 'co 5m');
  assert.equal(scheduleText(observer({ interval_secs: 7200 })), 'co 2h');
  assert.equal(intervalText(90), 'co 90s');
  assert.equal(scheduleText(observer({ interval_secs: undefined, cron: '*/10 * * * *' })), 'cron */10 * * * * (UTC)');
  assert.equal(modeText(undefined), 'brak obserwatorów');
  assert.match(modeText('always'), /^always/);
  assert.deepEqual(entryObserverParts({ observer: 'x' }), [{ text: 'jeszcze nie sprawdzony', role: 'done' }]);
  const parts = entryObserverParts({
    observer: 'x', running: true,
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

test('the create form asks for a name and one schedule', () => {
  const empty = createRequest({ ...observerForm(), interval: '' });
  assert.deepEqual(Object.keys(empty.problems).sort(), ['interval', 'name']);

  const interval = createRequest({ ...observerForm(), name: ' gitlab-mr ' });
  assert.deepEqual(interval.problems, {});
  assert.deepEqual(interval.body, { name: 'gitlab-mr', mode: 'on_episodes', interval_secs: 300 });

  const cron = createRequest({
    ...observerForm(null, { queue: 'mr-review', mode: 'always' }),
    name: 'jira-new', schedule: 'cron', cron: '*/10 * * * *',
  });
  assert.deepEqual(cron.body, { name: 'jira-new', mode: 'always', cron: '*/10 * * * *', queue: 'mr-review' });
  assert.ok(createRequest({ ...observerForm(), name: 'a', schedule: 'cron', cron: ' ' }).problems.cron);
});

test('the edit form sends only what changed', () => {
  const current = observer();
  const same = updateRequest(observerForm(current), current);
  assert.equal(same.problems.form, 'Nic się nie zmieniło.');

  const moved = updateRequest({ ...observerForm(current), queue: 'other', mode: 'always' }, current);
  assert.deepEqual(moved.body, { queue: 'other', mode: 'always' });

  const cron = updateRequest({ ...observerForm(current), schedule: 'cron', cron: '0 9 * * 1' }, current);
  assert.deepEqual(cron.body, { cron: '0 9 * * 1' });

  const back = observer({ interval_secs: undefined, cron: '0 9 * * 1' });
  assert.equal(observerForm(back).schedule, 'cron');
  assert.deepEqual(updateRequest({ ...observerForm(back), schedule: 'interval', interval: '600' }, back).body, { interval_secs: 600 });
  assert.ok(updateRequest({ ...observerForm(current), queue: '' }, current).problems.queue);
});

test('pause and resume follow the server rules', () => {
  assert.deepEqual(scheduleCommands('active'), ['pause']);
  assert.deepEqual(scheduleCommands('paused'), ['resume']);
  assert.deepEqual(scheduleCommands('error'), ['resume', 'pause']);
  assert.deepEqual(scheduleCommands('draft'), []);
});

test("a refusal shows the server's reason", () => {
  const mismatch = errorFrom(409, '{"code":"queue_mode_mismatch","message":"queue mr-review has mode on_episodes"}');
  assert.equal(refusalReason(mismatch), 'queue mr-review has mode on_episodes');
  const cron = errorFrom(400, '{"code":"invalid_observer","message":"a cron expression has five fields"}');
  assert.equal(refusalReason(cron), 'a cron expression has five fields');
  assert.equal(refusalReason(errorFrom(409, '{"code":"stale"}')), 'stale');
  assert.match(refusalReason(new ApiError({ kind: 'unavailable' })), /brak połączenia/);
  assert.match(refusalReason(errorFrom(404, '')), /nie znaleziono/);
});

test('an inline write keeps its refusal out of the notification', async () => {
  const clock = { now: () => 0, setTimeout: () => 1, clearTimeout: () => {} };
  const store = new Store({ tasks: () => new Promise(() => {}), harness: () => Promise.resolve(null) }, { clock });
  store.state.connection = { ...store.state.connection, state: 'live' };
  const refused = errorFrom(409, '{"code":"queue_mode_mismatch","message":"queue q has mode always"}');
  await assert.rejects(store.mutate(() => Promise.reject(refused), { inline: true }), refused);
  assert.equal(store.state.notification, null);
  await assert.rejects(store.mutate(() => Promise.reject(refused), { failure: 'nie' }), refused);
  assert.equal(store.state.notification.kind, 'error');
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

test('a script dialog is changed only by text that differs from the text it opened with', () => {
  assert.equal(scriptChanged(null, 'echo 1\n'), false, 'nothing typed yet');
  assert.equal(scriptChanged('echo 1\n', 'echo 1\n'), false, 'typed back to the saved text');
  assert.equal(scriptChanged('echo 2\n', 'echo 1\n'), true);
  assert.equal(scriptChanged('', undefined), false, 'empty create form');
  assert.equal(scriptChanged('echo 1', undefined), true, 'text typed after a failed read');
  assert.equal(hasUnsavedScript(), false, 'no dialog is open');
});

test('closing a dialog with an unsaved script asks first and stays open on no', () => {
  const asked = [];
  let closed = 0;
  const onClose = () => { closed += 1; };

  assert.equal(closeAfterAsking(true, onClose, (question) => { asked.push(question); return false; }), false);
  assert.equal(closed, 0, 'the edit is kept');
  assert.equal(closeAfterAsking(true, onClose, () => true), true);
  assert.equal(closed, 1);
  assert.equal(closeAfterAsking(false, onClose, () => { throw new Error('no question without changes'); }), true);
  assert.equal(closed, 2);
  assert.deepEqual(asked, [DISCARD_SCRIPT]);
});

/** A stand-in for a DOM element: `closest` and `querySelector` find `dialog` when given. */
function element({ inside = null, holds = null } = {}) {
  return {
    closest: (selector) => (selector === '[role="dialog"]' ? inside : null),
    querySelector: (selector) => (selector.endsWith('[role="dialog"]') ? holds : null),
  };
}

test('the queues page keys wait while a dialog is open, also with focus on the page body', () => {
  const dialog = { focus() {} };
  const body = element();
  const page = (open) => ({ querySelector: (selector) => (open && selector === '.overlay [role="dialog"]' ? dialog : null) });

  assert.equal(queuesKeysHeld(body, page(false)), false, 'no dialog: Escape and Q act');
  assert.equal(queuesKeysHeld(body, page(true)), true, 'overlay click left focus on the body');
  assert.equal(queuesKeysHeld(element({ inside: dialog }), page(true)), true, 'the key comes from the dialog');
  assert.equal(queuesKeysHeld(null, page(true)), true);
  assert.equal(queuesKeysHeld(body, undefined), false, 'no document');
});

test('keeping the edit gives focus back to the dialog that asked', () => {
  const dialog = { name: 'dialog' };
  const other = { name: 'other' };
  const page = { querySelector: () => other };

  assert.equal(dialogNear(element({ inside: dialog }), page), dialog, '✕ or Anuluj inside the dialog');
  assert.equal(dialogNear(element({ holds: dialog }), page), dialog, 'the overlay around the dialog');
  assert.equal(dialogNear(undefined, page), other, 'Escape: the open dialog of the page');
  assert.equal(dialogNear(undefined, undefined), null);
});

test('keeping the edit leaves the focus in a field of the dialog, else gives it to the dialog', () => {
  const textarea = { name: 'textarea' };
  const body = { name: 'body' };
  const focused = [];
  const dialog = { contains: (node) => node === textarea, focus: () => focused.push('dialog') };

  keepFocusIn(dialog, textarea);
  assert.deepEqual(focused, [], 'Escape in the script text: the caret stays');
  keepFocusIn(dialog, body);
  assert.deepEqual(focused, ['dialog'], 'a click on the overlay left the focus on the body');
  keepFocusIn(dialog, null);
  assert.deepEqual(focused, ['dialog', 'dialog']);
  keepFocusIn(null, body);
  assert.deepEqual(focused, ['dialog', 'dialog'], 'no dialog: nothing to focus');
});
