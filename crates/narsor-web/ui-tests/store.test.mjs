import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  Store, connectionText, mutationGate, nextWaiting, reconnectDelay, retryDelay, settleSelection, staleLabel,
} from '../ui/js/store.js';
import { ApiError } from '../ui/js/api.js';

test('retryDelay doubles from 1 s up to 10 s', () => {
  assert.deepEqual([1, 2, 3, 4, 5, 6].map(retryDelay), [1000, 2000, 4000, 8000, 10000, 10000]);
});

test('reconnectDelay doubles from 250 ms up to 15 s', () => {
  assert.deepEqual([0, 1, 2, 3, 7, 9].map(reconnectDelay), [0, 250, 500, 1000, 15000, 15000]);
});

test('settleSelection keeps the task, else takes the row below, else above', () => {
  assert.equal(settleSelection(['a', 'b', 'c'], ['a', 'b', 'c'], 'b'), 'b');
  assert.equal(settleSelection(['a', 'b', 'c'], ['a', 'c'], 'b'), 'c');
  assert.equal(settleSelection(['a', 'b', 'c'], ['a'], 'c'), 'a');
  assert.equal(settleSelection([], ['x'], null), 'x');
  assert.equal(settleSelection(['a'], [], 'a'), null);
});

test('nextWaiting wraps round and checks the selected row last', () => {
  const rows = [
    { task_id: 'a', state: 'human_needed' },
    { task_id: 'b', state: 'running' },
    { task_id: 'c', state: 'error' },
  ];
  assert.equal(nextWaiting(rows, 'a'), 'c');
  assert.equal(nextWaiting(rows, 'c'), 'a');
  assert.equal(nextWaiting([{ task_id: 'b', state: 'running' }], 'b'), null);
});

test('the mutation gate blocks writes unless live, with the age of the data', () => {
  assert.equal(mutationGate({ state: 'live', lastData: 0 }, 0), null);
  assert.equal(
    mutationGate({ state: 'disconnected', lastData: 0 }, 40_000),
    'brak połączenia (stan sprzed 40s): akcja zmieniająca stan jest zablokowana',
  );
  assert.equal(
    mutationGate({ state: 'connecting', lastData: null }, 0),
    'łączę z backendem: akcja zmieniająca stan jest zablokowana',
  );
});

test('connection texts follow the TUI', () => {
  assert.equal(connectionText({ state: 'connecting', name: 'local' }, 0).text, 'łączę z local');
  assert.equal(connectionText({ state: 'retrying', retryAt: 1500 }, 0).text, 'ponawiam (2s)');
  assert.equal(connectionText({ state: 'retrying', retryAt: 0 }, 10).text, 'ponawiam (1s)');
  assert.equal(staleLabel({ state: 'live', lastData: 0 }, 99_000), null);
});

function fakeClock() {
  let now = 0;
  let next = 1;
  const timers = new Map();
  return {
    now: () => now,
    setTimeout(fn, ms) { const id = next++; timers.set(id, { at: now + ms, fn }); return id; },
    clearTimeout(id) { timers.delete(id); },
    pending: () => [...timers.values()].map((timer) => timer.at - now).sort((a, b) => a - b),
    advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) if (timer.at <= now) { timers.delete(id); timer.fn(); }
    },
  };
}

function fakeApi(listings) {
  let calls = 0;
  return {
    calls: () => calls,
    async tasks() {
      const next = listings[Math.min(calls, listings.length - 1)];
      calls += 1;
      if (next instanceof Error) throw next;
      return next;
    },
    async harness() { return { effective: 'claude-code', override: null }; },
    async capabilities() { return { connection_name: 'local' }; },
    async usage() { return { harnesses: [] }; },
    async *streamEvents() { /* never yields; ends */ await new Promise(() => {}); },
  };
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

test('a failed read backs off and shows the countdown once data arrived', async () => {
  const clock = fakeClock();
  const offline = new ApiError({ kind: 'unavailable' });
  const api = fakeApi([{ tasks: [{ task_id: 'a', state: 'running' }], cursor: '' }, offline, offline]);
  const store = new Store(api, { clock });
  await store.refresh();
  assert.equal(store.state.connection.state, 'live');
  assert.equal(store.state.selectedTaskId, 'a');

  clock.advance(10_000);
  await flush();
  assert.equal(store.state.connection.state, 'retrying');
  assert.equal(store.state.connection.retryAt - clock.now(), 1000);
  assert.ok(mutationGate(store.state.connection, clock.now()));
  store.stop();
});

test('a task that left the list moves the selection and warns', async () => {
  const clock = fakeClock();
  const api = fakeApi([
    { tasks: [{ task_id: 'a', state: 'running', title: 'A' }, { task_id: 'b', state: 'running', title: 'B' }], cursor: '' },
    { tasks: [{ task_id: 'b', state: 'running', title: 'B' }], cursor: '' },
  ]);
  const store = new Store(api, { clock });
  await store.refresh();
  assert.equal(store.state.selectedTaskId, 'a');
  await store.refresh();
  assert.equal(store.state.selectedTaskId, 'b');
  assert.equal(store.state.notification.text, 'A: zniknęło z listy (zarchiwizowane) — zaznaczono B');
  store.stop();
});

test('a 401 stops the schedule and asks for a token', async () => {
  const clock = fakeClock();
  const store = new Store(fakeApi([new ApiError({ status: 401, kind: 'unauthorized' })]), { clock });
  await store.refresh();
  assert.equal(store.state.auth, 'unauthorized');
  store.stop();
});
