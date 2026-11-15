// The state of the application and the schedule that keeps it fresh: the
// task list (at start, every 10 s, 400 ms after a stream event, after each
// write, with a back-off after failures), the event stream, the usage
// limits and the connection state. The rules are the TUI's
// (`docs/sources/narsor-tui-inventory-part-a.md`, A.2, A.3, A.7, A.8).
//
// The clock comes from outside (`clock`), so `ui-tests/` can drive it.

import { isDone, matchesFilter, needsOperator, shortAge, stateIcon, titleOf } from './model.js';

export const REFRESH_INTERVAL_MS = 10_000;
export const DEBOUNCE_MS = 400;
export const USAGE_PERIOD_MS = 15_000;
export const NOTIFICATION_MS = 6_000;

/** The delay before list read number `failures + 1`: 1, 2, 4, 8, 10, 10 … s. */
export function retryDelay(failures) {
  if (failures <= 0) return 0;
  return Math.min(1000 * 2 ** (failures - 1), 10_000);
}

/** The delay before stream connection attempt `failures + 1`: 0, 250 ms × 2ⁿ, at most 15 s. */
export function reconnectDelay(failures) {
  if (failures <= 0) return 0;
  return Math.min(250 * 2 ** (failures - 1), 15_000);
}

/** The text and role of the connection state. */
export function connectionText(connection, now) {
  switch (connection.state) {
    case 'live':
      return { icon: '●', text: 'live', role: 'live' };
    case 'retrying': {
      const seconds = Math.max(1, Math.ceil((connection.retryAt - now) / 1000));
      return { icon: '◌', text: `ponawiam (${seconds}s)`, role: 'attention' };
    }
    case 'disconnected':
      return { icon: '✖', text: 'brak połączenia', role: 'problem' };
    default:
      return { icon: '◌', text: `łączę z ${connection.name}`, role: 'attention' };
  }
}

/** `stan sprzed 40s` while not live after data arrived; else `null`. */
export function staleLabel(connection, now) {
  if (connection.state === 'live' || connection.lastData === null) return null;
  return `stan sprzed ${shortAge(Math.max(0, Math.floor((now - connection.lastData) / 1000)))}`;
}

/** `null` when writes may run; else the warning that blocks them. */
export function mutationGate(connection, now) {
  if (connection.state === 'live') return null;
  const state = {
    retrying: 'ponawiam połączenie',
    disconnected: 'brak połączenia',
  }[connection.state] ?? 'łączę z backendem';
  const age = staleLabel(connection, now);
  return `${state}${age ? ` (${age})` : ''}: akcja zmieniająca stan jest zablokowana`;
}

/** The rows the list shows, in server order. */
export function shownTasks(tasks, filter, hideDone) {
  return tasks.filter((task) => (!hideDone || !isDone(task)) && matchesFilter(task, filter));
}

/**
 * The selection after the shown rows changed from `before` to `after`
 * (arrays of task ids): the same task when still shown; else the first
 * row that was below it, else the nearest above, else the first row.
 */
export function settleSelection(before, after, selected) {
  if (after.length === 0) return null;
  if (selected !== null && after.includes(selected)) return selected;
  const shown = new Set(after);
  const index = before.indexOf(selected);
  if (index >= 0) {
    for (let i = index + 1; i < before.length; i += 1) if (shown.has(before[i])) return before[i];
    for (let i = index - 1; i >= 0; i -= 1) if (shown.has(before[i])) return before[i];
  }
  return after[0];
}

/** The next task after `selected` that waits for the operator, wrapping round. */
export function nextWaiting(rows, selected) {
  const start = rows.findIndex((task) => task.task_id === selected);
  for (let step = 1; step <= rows.length; step += 1) {
    const task = rows[(start + step + rows.length) % rows.length];
    if (needsOperator(stateIcon(task))) return task.task_id;
  }
  return null;
}

const browserClock = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
  clearTimeout: (id) => globalThis.clearTimeout(id),
};

export class Store {
  constructor(api, { clock = browserClock, connectionName = 'serwer' } = {}) {
    this.api = api;
    this.clock = clock;
    this.listeners = new Set();
    this.state = {
      tasks: null,
      cursor: null,
      selectedTaskId: null,
      filter: '',
      hideDone: false,
      tab: 'conversation',
      connection: { state: 'connecting', name: connectionName, lastData: null, retryAt: null },
      harness: null,
      harnessPending: null,
      capabilities: null,
      usage: null,
      notification: null,
      auth: null,
      loading: false,
    };
    this.timers = {};
    this.failures = 0;
    this.streamFailures = 0;
    this.stream = null;
    this.refreshing = false;
    this.refreshAgain = false;
    this.stopped = false;
  }

  // Subscription

  subscribe(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  set(patch) {
    this.state = { ...this.state, ...patch };
    for (const listener of this.listeners) listener(this.state);
  }

  // Lifecycle

  start() {
    this.refresh();
    this.readUsage();
  }

  stop() {
    this.stopped = true;
    for (const id of Object.values(this.timers)) this.clock.clearTimeout(id);
    this.stream?.abort();
  }

  schedule(name, ms, fn) {
    this.clock.clearTimeout(this.timers[name]);
    this.timers[name] = this.clock.setTimeout(() => {
      delete this.timers[name];
      if (!this.stopped) fn();
    }, ms);
  }

  /** Asks for a list read `DEBOUNCE_MS` from now; later asks in the window join it. */
  requestRefresh() {
    if (this.timers.debounce !== undefined) return;
    this.schedule('debounce', DEBOUNCE_MS, () => this.refresh());
  }

  // List read

  async refresh() {
    if (this.refreshing) {
      this.refreshAgain = true;
      return;
    }
    this.refreshing = true;
    this.set({ loading: true });
    try {
      const [listing, harness] = await Promise.all([
        this.api.tasks(false),
        this.api.harness().catch(() => this.state.harness),
      ]);
      this.failures = 0;
      this.applyListing(listing, harness);
      if (!this.state.capabilities) {
        this.api.capabilities().then(
          (capabilities) => this.set({
            capabilities,
            connection: { ...this.state.connection, name: capabilities.connection_name ?? this.state.connection.name },
          }),
          () => {},
        );
      }
      this.ensureStream();
      this.schedule('refresh', REFRESH_INTERVAL_MS, () => this.refresh());
    } catch (error) {
      this.onFailure(error);
    } finally {
      this.refreshing = false;
      this.set({ loading: false });
      if (this.refreshAgain && !this.stopped) {
        this.refreshAgain = false;
        this.refresh();
      }
    }
  }

  applyListing(listing, harness) {
    const now = this.clock.now();
    const old = this.state.tasks ?? [];
    const tasks = listing.tasks ?? [];
    const { filter, hideDone } = this.state;
    const before = shownTasks(old, filter, hideDone).map((task) => task.task_id);
    const after = shownTasks(tasks, filter, hideDone).map((task) => task.task_id);
    const selected = this.state.selectedTaskId;
    const next = settleSelection(before, after, selected);
    const patch = {
      tasks,
      cursor: listing.cursor ?? '',
      selectedTaskId: next,
      harness,
      harnessPending: null,
      connection: { ...this.state.connection, state: 'live', lastData: now, retryAt: null },
    };
    this.set(patch);
    if (selected !== null && old.some((task) => task.task_id === selected) && !tasks.some((task) => task.task_id === selected)) {
      const gone = old.find((task) => task.task_id === selected);
      const chosen = tasks.find((task) => task.task_id === next);
      this.notify('warning', `${titleOf(gone)}: zniknęło z listy (zarchiwizowane)${chosen ? ` — zaznaczono ${titleOf(chosen)}` : ''}`);
    }
  }

  onFailure(error) {
    if (error?.kind === 'unauthorized' || error?.kind === 'forbidden') {
      this.set({ auth: error.kind });
      this.stop();
      return;
    }
    this.failures += 1;
    const delay = retryDelay(this.failures);
    const connection = this.state.connection;
    if (!error?.transient) {
      this.set({ connection: { ...connection, state: 'disconnected', retryAt: null } });
    } else if (connection.lastData !== null) {
      this.set({ connection: { ...connection, state: 'retrying', retryAt: this.clock.now() + delay } });
    }
    this.schedule('refresh', delay, () => this.refresh());
  }

  // Event stream

  ensureStream() {
    if (this.stream || this.state.cursor === null) return;
    const controller = new AbortController();
    this.stream = controller;
    this.follow(controller);
  }

  async follow(controller) {
    let delivered = false;
    try {
      for await (const envelope of this.api.streamEvents(this.state.cursor, controller.signal)) {
        delivered = true;
        this.streamFailures = 0;
        this.set({
          cursor: envelope.cursor,
          connection: { ...this.state.connection, state: 'live', lastData: this.clock.now(), retryAt: null },
        });
        this.requestRefresh();
      }
      if (controller.signal.aborted) return;
      // The server ended the stream (token expiry, a proxy): open it again.
      this.stream = null;
      if (delivered) this.streamFailures = 0;
      else this.streamFailures += 1;
      this.schedule('stream', reconnectDelay(this.streamFailures), () => this.ensureStream());
    } catch (error) {
      this.stream = null;
      if (controller.signal.aborted) return;
      if (error?.transient) {
        this.streamFailures += 1;
        const delay = reconnectDelay(this.streamFailures);
        this.set({ connection: { ...this.state.connection, state: 'retrying', retryAt: this.clock.now() + delay } });
        this.schedule('stream', delay, () => this.ensureStream());
      } else if (error?.kind === 'unauthorized' || error?.kind === 'forbidden') {
        this.onFailure(error);
      } else {
        // Ended for good; the next list read gives a new cursor and opens it again.
        this.set({ connection: { ...this.state.connection, state: 'disconnected', retryAt: null } });
        this.notify('error', `strumień zdarzeń zakończony: ${error?.message ?? error}`);
      }
    }
  }

  // Usage limits

  async readUsage() {
    try {
      const usage = await this.api.usage();
      this.set({ usage: usage?.harnesses?.length ? usage : null });
    } catch (error) {
      if (error?.kind === 'unauthorized' || error?.kind === 'forbidden') return;
      this.set({ usage: null });
    }
    this.schedule('usage', USAGE_PERIOD_MS, () => this.readUsage());
  }

  // Notifications

  notify(kind, text) {
    const notification = { kind, text, id: this.clock.now() + Math.random() };
    this.set({ notification });
    this.schedule('notification', NOTIFICATION_MS, () => {
      if (this.state.notification === notification) this.set({ notification: null });
    });
  }

  // View state

  shown() {
    return shownTasks(this.state.tasks ?? [], this.state.filter, this.state.hideDone);
  }

  select(taskId) {
    this.set({ selectedTaskId: taskId });
  }

  setView(patch) {
    const before = this.shown().map((task) => task.task_id);
    this.state = { ...this.state, ...patch };
    const after = this.shown().map((task) => task.task_id);
    this.set({ selectedTaskId: settleSelection(before, after, this.state.selectedTaskId) });
  }

  step(delta) {
    const rows = this.shown();
    if (!rows.length) return;
    const index = rows.findIndex((task) => task.task_id === this.state.selectedTaskId);
    const next = Math.min(rows.length - 1, Math.max(0, (index < 0 ? 0 : index) + delta));
    this.select(rows[next].task_id);
  }

  selectNextWaiting() {
    const next = nextWaiting(this.shown(), this.state.selectedTaskId);
    if (next === null) this.notify('warning', 'żadne zadanie nie czeka na Ciebie');
    else this.select(next);
  }

  // Writes

  /**
   * Runs a write when the connection is live. Shows `success` (a text or a
   * function of the result) and reads the list again; on a refusal shows the
   * reason. Resolves to the result, or `undefined` when it did not run.
   */
  async mutate(run, { success, failure = 'nie udało się' } = {}) {
    const blocked = mutationGate(this.state.connection, this.clock.now());
    if (blocked) {
      this.notify('warning', blocked);
      return undefined;
    }
    try {
      const result = await run(this.api);
      const text = typeof success === 'function' ? success(result) : success;
      if (text) this.notify('success', text);
      return result;
    } catch (error) {
      if (error?.kind === 'unauthorized' || error?.kind === 'forbidden') {
        this.onFailure(error);
      } else {
        this.notify('error', `${failure} — ${error?.message ?? error}`);
      }
      throw error;
    } finally {
      this.refresh();
    }
  }
}
