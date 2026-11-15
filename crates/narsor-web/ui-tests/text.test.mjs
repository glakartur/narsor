import { test } from 'node:test';
import assert from 'node:assert/strict';
import { harnessShort, harnessText, usageGroups } from '../ui/js/text.js';

test('usage groups follow the TUI format and colour by status', () => {
  const now = '2026-11-13T10:00:00Z';
  const groups = usageGroups({
    harnesses: [
      { harness: 'claude-code', limits: [
        { window: '5h', utilization: 0.01, resets_at: '2026-11-13T14:10:00Z', status: 'allowed' },
        { window: '7d', utilization: 0.9, resets_at: '2026-11-15T13:00:00Z', status: 'allowed_warning' },
      ] },
      { harness: 'x-new', limits: [{ window: '5h', utilization: 1, resets_at: '2026-11-13T09:00:00Z', status: 'rejected' }] },
    ],
  }, now);
  assert.deepEqual(groups, [
    { name: 'claude', limits: [{ text: '5h 1% ↻4h10m', role: null }, { text: '7d 90% ↻2d3h', role: 'attention' }] },
    { name: 'x-new', limits: [{ text: '5h 100% ↻0m', role: 'problem' }] },
  ]);
});

test('harness names and header text', () => {
  assert.equal(harnessShort('github-copilot'), 'copilot');
  assert.equal(harnessText(null, null), 'harness ?');
  assert.equal(harnessText({ effective: 'codex', override: null }, null), 'harness codex (konfiguracja)');
  assert.equal(harnessText({ effective: 'codex', override: 'codex' }, null), 'harness codex');
  assert.equal(harnessText({ effective: 'codex', override: null }, 'opencode'), 'harness opencode…');
});

import { eventDetail, groupEntries, pagingDone, sumUsage, usageLine, attemptsOf, duration } from '../ui/js/text.js';

test('event details follow the TUI texts', () => {
  assert.equal(eventDetail({ type: 'state_exited', state_id: 'work', output_label: 'done' }), 'state_exited  work → done');
  assert.equal(eventDetail({ type: 'human_input_received', close_reason: 'cancel', answered_by: 'conductor' }), 'human_input_received  closed by cancel (conductor)');
  assert.equal(eventDetail({ type: 'human_input_received', output_label: 'ok' }), 'human_input_received  operator → ok');
  assert.equal(eventDetail({ type: 'agent_session_started', harness: 'codex', action_execution_id: '0123456789' }), 'agent_session_started  codex (attempt 01234567)');
  assert.equal(eventDetail({ type: 'conductor_decision', kind: 'retry' }), 'conductor_decision  retry (low)');
  assert.equal(eventDetail({ type: 'permission_denied_ignored', tool_name: 'Bash', summary: 'rm -rf' }), 'odmowa pominięta · agent · Bash · rm -rf');
  assert.equal(eventDetail({ type: 'variable_set_by_workflow', variable: 'x', value: { a: 1 }, state_id: 's', output_label: 'o' }), 'variable_set  x = {"a":1} (s, o)');
  assert.equal(eventDetail({ type: 'brand_new' }), 'unknown_event');
});

test('usage line matches the TUI example', () => {
  const half = {
    total: { input: 152, output: 36000 }, subagents: { input: 50, output: 10000 }, subagent_count: 1,
    cost_usd: 17.075, conductor_total: { input: 152, output: 36000 }, conductor_cost_usd: 17.075,
  };
  const line = usageLine(sumUsage([{ usage: half }, { usage: half }, { usage: null }]));
  assert.equal(line, 'razem $68.30 · ✨ $34.15 ↑304 ↓72k (◈ 2 ↑100 ↓20k) · ⚡ $34.15 ↑304 ↓72k');
  assert.equal(usageLine(sumUsage([{}])), null);
});

test('conversation paging stops like the TUI', () => {
  assert.equal(pagingDone({ status: 'no_record' }, 0), true);
  assert.equal(pagingDone({ status: 'recorded', entries: [], next_entry: 0 }, 0), true);
  assert.equal(pagingDone({ status: 'recorded', entries: [{}], next_entry: 5, closed: true }, 0), true);
  assert.equal(pagingDone({ status: 'recorded', entries: [{}], next_entry: 5 }, 5), true);
  assert.equal(pagingDone({ status: 'recorded', entries: [{}], next_entry: 5 }, 0), false);
});

test('subagent entries become one group with an error count', () => {
  const sub = { id: 's1', name: 'Explore' };
  const items = groupEntries([
    { entry_number: 1 },
    { entry_number: 2, subagent: sub, elements: [{ kind: 'tool_result', status: 'error' }] },
    { entry_number: 3, subagent: sub, elements: [] },
    { entry_number: 4 },
  ]);
  assert.equal(items.length, 3);
  assert.equal(items[1].entries.length, 2);
  assert.equal(items[1].errors, 1);
});

test('attempts and durations', () => {
  assert.deepEqual(attemptsOf({ conversation: { state: 'recorded' }, action_execution_id: 'a' }), ['a']);
  assert.deepEqual(attemptsOf({ conversation: { state: 'no_record' }, attempts: ['a'] }), []);
  assert.equal(duration(185_000), '3m 05s');
  assert.equal(duration(3_720_000), '1h 02m');
});
