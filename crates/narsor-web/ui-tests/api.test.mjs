import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Api, ApiError, errorFrom, kindOf, splitFrames } from '../ui/js/api.js';

test('splitFrames keeps a frame cut in two chunks for the next read', () => {
  const first = splitFrames('event: task\nid: a:1\ndata: {"cursor":"a:1"');
  assert.deepEqual(first.payloads, []);
  const second = splitFrames(first.rest + '}\n\n');
  assert.deepEqual(second.payloads, ['{"cursor":"a:1"}']);
  assert.equal(second.rest, '');
});

test('splitFrames skips comments and reads several frames at once', () => {
  const { payloads, rest } = splitFrames(': keep-alive\n\ndata: 1\n\n:x\n\ndata: 2\n\ndata: 3');
  assert.deepEqual(payloads, ['1', '2']);
  assert.equal(rest, 'data: 3');
});

test('splitFrames accepts CRLF line ends', () => {
  assert.deepEqual(splitFrames('data: x\r\n\r\n').payloads, ['x']);
});

test('kindOf maps the statuses of the API', () => {
  assert.equal(kindOf(401), 'unauthorized');
  assert.equal(kindOf(403), 'forbidden');
  assert.equal(kindOf(404), 'not_found');
  assert.equal(kindOf(400), 'request');
  assert.equal(kindOf(415), 'request');
  assert.equal(kindOf(422), 'request');
  assert.equal(kindOf(409), 'refused');
  assert.equal(kindOf(503), 'degraded');
  assert.equal(kindOf(500), 'server');
});

test('errorFrom reads a JSON body, a text body and an empty one', () => {
  const json = errorFrom(409, '{"code":"stale_generation","message":"moved on"}');
  assert.equal(json.code, 'stale_generation');
  assert.equal(json.message, 'moved on');
  assert.equal(json.kind, 'refused');

  const text = errorFrom(422, 'Failed to deserialize the JSON body');
  assert.equal(text.code, null);
  assert.equal(text.message, 'Failed to deserialize the JSON body');

  const empty = errorFrom(401, '');
  assert.equal(empty.kind, 'unauthorized');
  assert.equal(empty.message, 'HTTP 401');
});

function fakeFetch(status, body, seen = []) {
  return async (url, init) => {
    seen.push({ url: String(url), init });
    return new Response(body, { status });
  };
}

test('request sends the bearer token and resolves 202 as success', async () => {
  const seen = [];
  const api = new Api('http://h/', 'tok', fakeFetch(202, '{"replayed":false}', seen));
  const result = await api.comment('task-1', 'hi', 7, 'key');
  assert.deepEqual(result, { replayed: false });
  assert.equal(seen[0].url, 'http://h/control/v1/tasks/task-1/comment');
  assert.equal(seen[0].init.headers.Authorization, 'Bearer tok');
  assert.deepEqual(JSON.parse(seen[0].init.body), { text: 'hi', idempotency_key: 'key', expected_generation: 7 });
});

test('request keeps a path prefix of the base address', async () => {
  const seen = [];
  const api = new Api('https://h/narsor/', 't', fakeFetch(200, '{"tasks":[],"cursor":""}', seen));
  await api.tasks(false);
  assert.equal(seen[0].url, 'https://h/narsor/control/v1/tasks?archived=false');
});

test('request turns a network failure into an unavailable error', async () => {
  const api = new Api('http://h/', 't', async () => { throw new TypeError('Failed to fetch'); });
  await assert.rejects(api.tasks(), (error) => error instanceof ApiError && error.kind === 'unavailable' && error.transient);
});

test('streamEvents yields envelopes across chunks', async () => {
  const encoder = new TextEncoder();
  const chunks = ['data: {"cursor":"a:1","event":{}}\n', '\n: keep-alive\n\ndata: {"cursor":"a:2","event":{}}\n\n'];
  const body = new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(encoder.encode(chunk));
      controller.close();
    },
  });
  const seen = [];
  const api = new Api('http://h/', 't', async (url) => { seen.push(String(url)); return new Response(body, { status: 200 }); });
  const cursors = [];
  for await (const envelope of api.streamEvents('a:0,b:3')) cursors.push(envelope.cursor);
  assert.deepEqual(cursors, ['a:1', 'a:2']);
  assert.equal(seen[0], 'http://h/control/v1/events/stream?cursor=a%3A0%2Cb%3A3');
});

test('streamEvents throws when the stream cannot open', async () => {
  const api = new Api('http://h/', 't', fakeFetch(400, '{"code":"invalid_cursor","message":"x"}'));
  await assert.rejects(async () => { for await (const _ of api.streamEvents('bad')) { /* none */ } },
    (error) => error.code === 'invalid_cursor' && error.kind === 'request');
});
