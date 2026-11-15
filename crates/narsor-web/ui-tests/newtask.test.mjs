import { test } from 'node:test';
import assert from 'node:assert/strict';
import { branchField, buildLaunch, formFields, inputValues, problemOf, repositoryField } from '../ui/js/newtask.js';

const repositories = [{ id: 'narsor', default_branch: 'main' }, { id: 'docs', default_branch: 'master' }];
const workflow = {
  name: 'feature',
  version: 3,
  workspaces: [{ name: 'code' }, { name: 'docs', base_branch: 'develop' }],
  inputs: [
    { name: 'ticket_id', type: 'string', required: true },
    { name: 'count', type: 'integer', required: true, default: 2 },
    { name: 'dry_run', type: 'boolean' },
    { name: 'description', type: 'text' },
  ],
};

test('fields stand in the TUI order with the TUI rules', () => {
  const fields = formFields(workflow, repositories);
  assert.deepEqual(fields.map((field) => field.name), [
    'Ticket', 'Tytuł',
    repositoryField('code'), branchField('code'), repositoryField('docs'), branchField('docs'),
    'ticket_id', 'count', 'dry_run', 'description',
  ]);
  const byName = Object.fromEntries(fields.map((field) => [field.name, field]));
  assert.equal(byName[branchField('code')].initial, 'main');
  assert.equal(byName[branchField('docs')].initial, 'develop');
  assert.equal(byName.count.required, false);
  assert.equal(byName.count.initial, '2');
  assert.equal(byName.dry_run.initial, 'false');
  assert.equal(byName.description.label, 'Opis');
  assert.equal(byName.description.type, 'multiline');
});

test('problems: required, length, integer, select', () => {
  const fields = Object.fromEntries(formFields(workflow, repositories).map((field) => [field.name, field]));
  assert.match(problemOf(fields.ticket_id, ' '), /wymagane/);
  assert.match(problemOf(fields.Ticket, 'x'.repeat(16)), /najwyżej 15/);
  assert.match(problemOf(fields.count, '2.5'), /liczby całkowitej/);
  assert.match(problemOf(fields.dry_run, 'yes'), /dozwoloną/);
  assert.equal(problemOf(fields.Tytuł, ''), null);
});

test('input values are typed; a blank one takes its default or is left out', () => {
  assert.deepEqual(
    inputValues(workflow.inputs, { ticket_id: ' ABC-1 ', count: '', dry_run: 'true', description: '' }),
    { ticket_id: 'ABC-1', count: 2, dry_run: true },
  );
});

test('buildLaunch makes the launch or reports problems', () => {
  const fields = formFields(workflow, repositories);
  assert.ok(buildLaunch(workflow, fields, {}).problems.ticket_id);
  const { launch } = buildLaunch(workflow, fields, { ticket_id: 'A-1', Ticket: ' NAR-1 ', [branchField('code')]: 'feat/x' });
  assert.deepEqual(launch, {
    workflow_id: 'feature',
    workflow_version: 3,
    inputs: { ticket_id: 'A-1', count: 2, dry_run: false },
    workspaces: {
      code: { repository: 'narsor', branch: 'feat/x', commit: null },
      docs: { repository: 'narsor', branch: 'develop', commit: null },
    },
    ticket: 'NAR-1',
  });
});

test('a workflow without repositories registered takes a free repository line', () => {
  const [, , repo] = formFields({ name: 'w', version: 1, workspaces: [{ name: 'code' }] }, []);
  assert.equal(repo.type, 'text');
  assert.equal(repo.required, true);
});
