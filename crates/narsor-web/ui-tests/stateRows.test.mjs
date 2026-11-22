import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { stateRows } from '../ui/js/stateRows.js';
import { loadExecutionState } from '../ui/js/ui/tabs/state.js';

const fixture = JSON.parse(readFileSync(new URL('./fixtures/state-rows.json', import.meta.url), 'utf8'));

test('flattening gives the rows of the shared fixture, as the Rust twin does', () => {
  assert.ok(fixture.cases.length > 0);
  for (const { name, state, rows } of fixture.cases) {
    assert.deepEqual(stateRows(state), rows, name);
  }
});

test('a multi-line string keeps its lines and counts them', () => {
  const section = (value) => ({ status: 'available', value });
  const rows = stateRows({
    context: section({ note: 'one\ntwo', title: 'one line' }),
    locals: section({}),
    conductor: { status: 'unreadable', reason: 'corrupt' },
  });
  assert.deepEqual(rows.slice(0, 2), [
    { path: 'context:note', value: 'one\ntwo', lines: 2 },
    { path: 'context:title', value: 'one line', lines: 1 },
  ]);
  assert.deepEqual(rows.at(-1), { path: 'conductor', value: '(corrupt)', lines: 1 });
});

const api = (answer) => ({ executionState: async () => answer });

test('a reader that is starting becomes the retryable error of useRead', async () => {
  await assert.rejects(
    loadExecutionState(api({ status: 'unavailable', reason: 'reader_starting', message: 'starting' }), 't1'),
    { code: 'conversation_reader_starting' },
  );
});

test('an archived answer stays data for the view', async () => {
  const answer = { status: 'unavailable', reason: 'archived', message: 'storage deleted' };
  assert.deepEqual(await loadExecutionState(api(answer), 't1'), answer);
});
