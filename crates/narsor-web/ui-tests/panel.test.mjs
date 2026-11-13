import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  checkEntry, deliver, fieldKind, fieldProblem, fillPlaceholders, panelActions, resolveSend, tagOf,
} from '../ui/js/panel.js';

const def = (plan, fields = []) => ({ episode: 'ep-1', entries: [{ label: 'x', plan, fields }] });

test('tagOf reads externally tagged enums', () => {
  assert.deepEqual(tagOf('answer'), { tag: 'answer', value: null });
  assert.deepEqual(tagOf({ output: { label: 'done' } }), { tag: 'output', value: { label: 'done' } });
  assert.deepEqual(fieldKind({ kind: { text: { multiline: true } } }), { type: 'multiline', options: [] });
  assert.deepEqual(fieldKind({ kind: { select: { options: ['a'] } } }), { type: 'select', options: ['a'] });
});

test('field checks follow the TUI', () => {
  assert.equal(fieldProblem({ name: 'a', required: true, kind: 'x' }, '  '), "Pole 'a' jest wymagane.");
  assert.equal(fieldProblem({ name: 'a', kind: 'x' }, ''), null);
  assert.equal(fieldProblem({ name: 's', kind: { select: { options: ['a'] } } }, 'b'), "'b' nie jest dozwoloną wartością pola 's'.");
  const { problems, values } = checkEntry({ fields: [{ name: 'a', kind: 'x' }, { name: 'b', kind: 'x', value: 'start' }] }, { a: '' });
  assert.deepEqual(problems, {});
  assert.deepEqual(values, [['b', 'start']]);
});

test('complete sets the label field and the chosen label', () => {
  const delivery = deliver(def({ complete: { label: 'approve', label_field: 'decision' } }), 0, [['note', 'ok']]);
  assert.deepEqual(delivery, { values: { note: 'ok', decision: 'approve' }, chosen_label: 'approve', episode: 'ep-1' });
  assert.equal(deliver(def({ complete: { label: null, label_field: null } }), 0, []).chosen_label, null);
});

test('answer needs an answer or an option', () => {
  assert.throws(() => deliver(def('answer'), 0, []), /Wpisz odpowiedź/);
  assert.deepEqual(deliver(def('answer'), 0, [['option', 'a']]).values, { option: 'a' });
});

test('output maps artifacts, variables, extra lines and the comment', () => {
  const definition = def({ output: { label: 'done' } }, [{ name: 'variables.count', proposed: 3 }]);
  const delivery = deliver(definition, 0, [
    ['artifacts.report', 'out/r.md'],
    ['variables.count', '3'],
    ['variables (klucz=wartość)', 'a = 1\n\nb=two'],
    ['komentarz', '  fine  '],
  ]);
  assert.deepEqual(delivery, {
    chosen_label: 'done',
    artifacts: { report: 'out/r.md' },
    variables: { count: 3, a: '1', b: 'two' },
    comment: 'fine',
    episode: 'ep-1',
  });
  assert.equal(deliver(definition, 0, [['variables.count', '4']]).variables.count, '4');
});

test('output refuses a bad line and a key given twice', () => {
  const definition = def({ output: { label: 'done' } });
  assert.throws(() => deliver(definition, 0, [['variables (klucz=wartość)', 'nokey']]), /klucz=wartość/);
  assert.throws(() => deliver(definition, 0, [['variables (klucz=wartość)', '=1']]), /nie ma klucza/);
  assert.throws(() => deliver(definition, 0, [['variables.a', '1'], ['variables (klucz=wartość)', 'a=2']]), /podany dwa razy/);
});

test('message, approve, return and entry limit', () => {
  assert.equal(deliver(def('message'), 0, [['Wiadomość dla agenta', 'hi']]).agent_message, 'hi');
  assert.deepEqual(deliver(def('approve'), 0, []), { action: 'approve', episode: 'ep-1' });
  assert.deepEqual(deliver(def('return'), 0, [['Komentarz dla agenta', 'redo']]), { comment: 'redo', action: 'resume', episode: 'ep-1' });
  assert.deepEqual(deliver(def({ entry_limit: { decision: 'continue' } }), 0, []).values, { entry_limit_decision: 'continue' });
});

test('permission pattern: empty or with a bracket is refused', () => {
  const plan = { permission: { decision: { decision: 'allow', scope: 'pattern' } } };
  assert.throws(() => deliver(def(plan), 0, []), /Wzorzec/);
  assert.throws(() => deliver(def(plan), 0, [['Wzorzec polecenia', 'npm (x)']]), /Wzorzec/);
  const ok = deliver(def(plan), 0, [['Wzorzec polecenia', ' npm run * '], ['Wiadomość dla agenta', ' ']]);
  assert.deepEqual(ok.permission_decision, { decision: 'allow', scope: 'pattern', pattern: 'npm run *' });
  const exact = deliver(def({ permission: { decision: { decision: 'deny', scope: 'exact_operation' } } }), 0, [['Wiadomość dla agenta', 'no']]);
  assert.deepEqual(exact.permission_decision, { decision: 'deny', scope: 'exact_operation', message: 'no' });
});

test('an unknown plan needs a newer client', () => {
  assert.throws(() => deliver(def({ brand_new: {} }), 0, []), /nowszej wersji/);
});

test('resolveSend for the three types', () => {
  assert.deepEqual(resolveSend({ type: 'owner', command: 'pause' }, {}), { route: 'owner', body: 'pause' });
  assert.deepEqual(
    resolveSend({ type: 'operator', command: { kind: 'select_step', target: 'work', note: '{{komentarz}}', nested: ['{{a}}', { b: '{{b}}' }] } }, { komentarz: ' fix ', b: '' }),
    { route: 'operator', body: { kind: 'select_step', target: 'work', note: 'fix', nested: [null, { b: null }] } },
  );
  assert.deepEqual(resolveSend({ type: 'comment', field: 'tekst' }, { tekst: ' hi ' }), { route: 'comment', body: 'hi' });
  assert.throws(() => resolveSend({ type: 'comment', field: 'tekst' }, { tekst: ' ' }), /wymagane/);
  assert.throws(() => resolveSend({ type: 'teleport' }, {}), /nowszej wersji/);
  assert.equal(fillPlaceholders('a {{b}}', { b: 'x' }), 'a {{b}}');
});

test('panel actions: only the panel surface, in order; none from an old server', () => {
  assert.equal(panelActions({}), null);
  const actions = panelActions({ actions: [
    { id: 'b', label: 'B', order: 20 },
    { id: 'c', label: 'C', order: 10, surface: 'conversation' },
    { id: 'a', label: 'A', order: 5 },
    { id: 'd', order: 1 },
  ] });
  assert.deepEqual(actions.map((action) => action.id), ['a', 'b']);
});
