// Flattens the state of an execution into display rows: one path and one text
// value per row. This is the twin of `src/client/state_rows.rs`; both read the
// cases of `ui-tests/fixtures/state-rows.json`. Change both together.

const SECTIONS = ['context', 'locals', 'conductor'];

const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

function push(rows, path, value) {
  rows.push({ path, value, lines: value.split('\n').length });
}

function flatten(rows, path, value) {
  if (isObject(value) && Object.keys(value).length > 0) {
    // The default sort compares UTF-16 code units.
    for (const key of Object.keys(value).sort()) flatten(rows, `${path}:${key}`, value[key]);
  } else if (Array.isArray(value) && value.some(isObject)) {
    value.forEach((item, index) => flatten(rows, `${path}:${index}`, item));
  } else if (typeof value === 'string') {
    push(rows, path, value);
  } else {
    push(rows, path, JSON.stringify(value));
  }
}

/**
 * Rows `{path, value, lines}` of an `available` execution state: `context`,
 * then `locals`, then `conductor`. A section that is not available is one row
 * named after the section, with its message or reason in parentheses.
 */
export function stateRows(state) {
  const rows = [];
  for (const name of SECTIONS) {
    const section = state[name];
    if (section.status === 'available') flatten(rows, name, section.value);
    else if (section.status === 'absent') push(rows, name, `(${section.message})`);
    else push(rows, name, `(${section.reason})`);
  }
  return rows;
}
