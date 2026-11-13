// The HTTP client of the control plane: the same routes the TUI calls
// (`docs/sources/narsor-owner-api-wire-reference.md`). Every call carries
// `Authorization: Bearer <token>`. Errors become `ApiError` with a `kind`
// the views can act on; a body is never parsed without a guard, because the
// server answers some errors with plain text or with nothing.

/** How long a read relayed through the runner may take. */
export const RELAYED_TIMEOUT_MS = 25_000;
/** How long any other call may take. */
export const DEFAULT_TIMEOUT_MS = 15_000;

/** A failed call. `kind` says what the views should do about it. */
export class ApiError extends Error {
  constructor({ status = 0, code = null, message = '', kind }) {
    super(message || code || (status ? `HTTP ${status}` : kind));
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.kind = kind;
  }

  /** Whether a retry may help: the network, a timeout or the server failed. */
  get transient() {
    return this.kind === 'unavailable' || this.kind === 'server' || this.kind === 'degraded';
  }
}

/** The `kind` of an HTTP status that is not a success. */
export function kindOf(status) {
  if (status === 401) return 'unauthorized';
  if (status === 403) return 'forbidden';
  if (status === 404) return 'not_found';
  if (status === 409) return 'refused';
  if (status === 503) return 'degraded';
  if (status === 400 || status === 415 || status === 422) return 'request';
  return 'server';
}

/** The error of a response with `status` and the raw `text` of its body. */
export function errorFrom(status, text) {
  let code = null;
  let message = '';
  const body = (text ?? '').trim();
  if (body) {
    try {
      const parsed = JSON.parse(body);
      if (parsed && typeof parsed === 'object') {
        code = typeof parsed.code === 'string' ? parsed.code : null;
        message = typeof parsed.message === 'string' ? parsed.message : '';
      }
    } catch {
      message = body.slice(0, 500);
    }
  }
  return new ApiError({ status, code, message, kind: kindOf(status) });
}

/** A new idempotency key. */
export function idempotencyKey() {
  return crypto.randomUUID();
}

/** A new task id, as the TUI makes them: `task-<uuid without dashes>`. */
export function newTaskId() {
  return `task-${crypto.randomUUID().replaceAll('-', '')}`;
}

/**
 * Splits the text of an event stream into frames. Returns the complete
 * frames' `data` payloads and the incomplete rest, to be prefixed to the
 * next chunk. Comments (`:`) and other fields are skipped.
 */
export function splitFrames(buffer) {
  const normalized = buffer.replace(/\r\n?/g, '\n');
  const parts = normalized.split('\n\n');
  const rest = parts.pop();
  const payloads = [];
  for (const frame of parts) {
    const data = frame
      .split('\n')
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.slice(line.startsWith('data: ') ? 6 : 5));
    if (data.length) payloads.push(data.join('\n'));
  }
  return { payloads, rest };
}

const path = encodeURIComponent;

export class Api {
  /**
   * @param {string} base  the address the API is under, ending in `/`
   * @param {string} token the bearer token
   * @param {typeof fetch} fetchImpl
   */
  constructor(base, token, fetchImpl = globalThis.fetch.bind(globalThis)) {
    this.base = base;
    this.token = token;
    this.fetch = fetchImpl;
  }

  url(route, query) {
    const url = new URL(route.replace(/^\//, ''), this.base);
    for (const [key, value] of Object.entries(query ?? {})) {
      if (value !== undefined && value !== null) url.searchParams.set(key, String(value));
    }
    return url;
  }

  /** One call. Resolves to the parsed body (or `null`), rejects with `ApiError`. */
  async request(method, route, { query, body, timeoutMs = DEFAULT_TIMEOUT_MS, signal } = {}) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(new DOMException('timeout', 'TimeoutError')), timeoutMs);
    const onAbort = () => controller.abort(signal.reason);
    signal?.addEventListener('abort', onAbort, { once: true });
    const headers = { Authorization: `Bearer ${this.token}`, Accept: 'application/json' };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    try {
      let response;
      try {
        response = await this.fetch(this.url(route, query), {
          method,
          headers,
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: controller.signal,
          cache: 'no-store',
          credentials: 'omit',
          referrerPolicy: 'no-referrer',
        });
      } catch (error) {
        if (signal?.aborted) throw error;
        throw new ApiError({ kind: 'unavailable', message: error?.message ?? 'brak sieci' });
      }
      const text = await response.text().catch(() => '');
      if (response.status >= 200 && response.status < 300) {
        if (!text.trim()) return null;
        try {
          return JSON.parse(text);
        } catch {
          throw new ApiError({ status: response.status, kind: 'server', message: 'odpowiedź nie jest JSON' });
        }
      }
      throw errorFrom(response.status, text);
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener('abort', onAbort);
    }
  }

  get(route, query, options) {
    return this.request('GET', route, { ...options, query });
  }

  post(route, body, options) {
    return this.request('POST', route, { ...options, body });
  }

  // Identity and settings
  whoami() { return this.get('/auth/v1/whoami'); }
  capabilities() { return this.get('/control/v1/capabilities'); }
  usage() { return this.get('/control/v1/usage'); }
  harness() { return this.get('/settings/agent-harness'); }
  setHarness(harness) { return this.request('PUT', '/settings/agent-harness', { body: { harness } }); }
  clearHarness() { return this.request('DELETE', '/settings/agent-harness'); }
  concurrencyLimit() { return this.get('/control/v1/concurrency-limit'); }
  setConcurrencyLimit(limit) { return this.request('PUT', '/control/v1/concurrency-limit', { body: { limit } }); }

  // Tasks
  tasks(archived = false) { return this.get('/control/v1/tasks', { archived }); }
  task(taskId) { return this.get(`/control/v1/tasks/${path(taskId)}`); }
  createTask(request) { return this.post('/control/v1/tasks', request); }
  command(taskId, command, expectedGeneration, key = idempotencyKey()) {
    return this.post(`/control/v1/tasks/${path(taskId)}/commands`, {
      command, idempotency_key: key, expected_generation: expectedGeneration,
    });
  }
  move(taskId, target, expectedGeneration) {
    return this.post(`/control/v1/tasks/${path(taskId)}/move`, { target, expected_generation: expectedGeneration });
  }
  input(taskId, pendingInputId, delivery, key = idempotencyKey()) {
    return this.post(`/control/v1/tasks/${path(taskId)}/input`, {
      pending_input_id: pendingInputId, idempotency_key: key, delivery,
    });
  }
  operatorCommand(taskId, command, expectedGeneration, key = idempotencyKey()) {
    return this.post(`/control/v1/tasks/${path(taskId)}/operator-commands`, {
      ...command, idempotency_key: key, expected_generation: expectedGeneration,
    });
  }
  comment(taskId, text, expectedGeneration, key = idempotencyKey()) {
    return this.post(`/control/v1/tasks/${path(taskId)}/comment`, {
      text, idempotency_key: key, expected_generation: expectedGeneration,
    });
  }

  // Task reads relayed through the runner
  timeline(taskId, signal) {
    return this.get(`/control/v1/tasks/${path(taskId)}/timeline`, undefined, { signal, timeoutMs: RELAYED_TIMEOUT_MS });
  }
  conversation(taskId, attemptId, from, signal, byteBudget) {
    return this.get(
      `/control/v1/tasks/${path(taskId)}/attempts/${path(attemptId)}/conversation`,
      { from, byte_budget: byteBudget },
      { signal, timeoutMs: RELAYED_TIMEOUT_MS },
    );
  }
  executionEvents(taskId, after, limit, signal) {
    return this.get(`/control/v1/tasks/${path(taskId)}/execution-events`, { after, limit }, { signal, timeoutMs: RELAYED_TIMEOUT_MS });
  }
  conductor(taskId, after, limit, signal) {
    return this.get(`/control/v1/tasks/${path(taskId)}/conductor`, { after, limit }, { signal, timeoutMs: RELAYED_TIMEOUT_MS });
  }

  // Workflows and repositories
  workflows() { return this.get('/control/v1/workflows'); }
  validateWorkflow(body) { return this.post('/control/v1/workflows/validate', body); }
  startTaskFromPath(body) { return this.post('/tasks', body); }
  repositories() { return this.get('/control/v1/repositories'); }
  addRepository(body) { return this.post('/control/v1/repositories', body); }
  removeRepository(id) { return this.request('DELETE', `/control/v1/repositories/${path(id)}`); }
  branches(id) { return this.get(`/control/v1/repositories/${path(id)}/branches`); }

  // History
  executions() { return this.get('/control/v1/executions'); }
  executionEventsLegacy(id, after) { return this.get(`/control/v1/executions/${path(id)}/events`, { after }); }
  executionTimeline(id) { return this.get(`/control/v1/executions/${path(id)}/timeline`); }
  executionConversation(id, attemptId, from, byteBudget) {
    return this.get(`/control/v1/executions/${path(id)}/attempts/${path(attemptId)}/conversation`, { from, byte_budget: byteBudget });
  }
  executionConductor(id) { return this.get(`/control/v1/executions/${path(id)}/conductor`); }
  executionConductorChat(id, after) { return this.get(`/control/v1/executions/${path(id)}/conductor/chat`, { after }); }
  recover(id, body) { return this.post(`/control/v1/executions/${path(id)}/recover`, body); }

  /**
   * The event stream from `cursor`: an async generator of envelopes
   * `{cursor, event}`. It ends when the server ends the stream; it throws
   * `ApiError` when the stream cannot open.
   */
  async *streamEvents(cursor, signal) {
    let response;
    try {
      response = await this.fetch(this.url('/control/v1/events/stream', { cursor: cursor ?? '' }), {
        headers: { Authorization: `Bearer ${this.token}`, Accept: 'text/event-stream' },
        signal,
        cache: 'no-store',
        credentials: 'omit',
        referrerPolicy: 'no-referrer',
      });
    } catch (error) {
      if (signal?.aborted) return;
      throw new ApiError({ kind: 'unavailable', message: error?.message ?? 'brak sieci' });
    }
    if (response.status !== 200) {
      throw errorFrom(response.status, await response.text().catch(() => ''));
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let rest = '';
    try {
      for (;;) {
        let chunk;
        try {
          chunk = await reader.read();
        } catch (error) {
          if (signal?.aborted) return;
          throw new ApiError({ kind: 'unavailable', message: error?.message ?? 'strumień przerwany' });
        }
        if (chunk.done) return;
        const split = splitFrames(rest + decoder.decode(chunk.value, { stream: true }));
        rest = split.rest;
        for (const payload of split.payloads) {
          let envelope;
          try {
            envelope = JSON.parse(payload);
          } catch {
            continue;
          }
          if (envelope && typeof envelope.cursor === 'string') yield envelope;
        }
      }
    } finally {
      reader.cancel().catch(() => {});
    }
  }
}
