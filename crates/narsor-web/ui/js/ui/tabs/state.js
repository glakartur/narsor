// *Stan*: the complete execution state of the task (context, locals,
// conductor state) as a flat list of `path: value` rows, read on demand.

import { html, useEffect, useRef, useState } from '../../lib.js';
import { stateRows } from '../../stateRows.js';
import { clock } from '../../text.js';
import { RUNNER_SILENT, useRead } from '../read.js';

// This tab reads only when the owner asks, so a silent runner is retried by
// "Odśwież", not by the next change of the task.
const RUNNER_SILENT_HERE = 'runner nie odpowiada — odśwież, aby spróbować ponownie';

/**
 * Reads the state of a task. A reader that is still starting becomes the
 * retryable error `useRead` already knows; every other `unavailable` answer
 * (archived, not started, …) is data that the view shows.
 */
export async function loadExecutionState(api, taskId, signal) {
  const answer = await api.executionState(taskId, signal);
  if (answer?.status === 'unavailable' && answer.reason === 'reader_starting') {
    throw Object.assign(new Error(answer.message || answer.reason), { code: 'conversation_reader_starting' });
  }
  return answer;
}

function Row({ row, open, onToggle }) {
  const clipped = useRef(null);
  const [wide, setWide] = useState(false);
  const foldable = row.lines > 1 || wide;

  // A one-line value is foldable when it does not fit the width. The clipped
  // element only exists while the row is folded, so an unfolded row keeps the
  // last measure.
  useEffect(() => {
    const measure = () => {
      const element = clipped.current;
      if (element) setWide(element.scrollWidth > element.clientWidth);
    };
    measure();
    window.addEventListener('resize', measure);
    return () => window.removeEventListener('resize', measure);
  }, [row.value, open]);

  const onKeyDown = (event) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    event.stopPropagation();
    onToggle();
  };
  const toggle = foldable ? { role: 'button', tabIndex: 0, 'aria-expanded': open, onClick: onToggle, onKeyDown } : {};
  const marker = !foldable ? '·' : open ? '⌄' : '›';

  return html`
    <div class=${`state-row${foldable ? ' foldable' : ''}`}>
      <div class="state-line" ...${toggle}>
        <span class="state-marker" aria-hidden="true">${marker}</span>
        <span class="state-path">${row.path}:</span>
        ${!open && html`<span class="state-value" ref=${clipped}>${row.value.split('\n')[0]}</span>`}
      </div>
      ${open && html`<pre class="state-full">${row.value}</pre>`}
    </div>`;
}

export function State({ api, task }) {
  const [refreshes, setRefreshes] = useState(0);
  const [unfolded, setUnfolded] = useState(() => new Set());
  const { data, error, loading } = useRead((signal) => loadExecutionState(api, task.task_id, signal), `${task.task_id}:${refreshes}`);
  const rows = data?.status === 'available' ? stateRows(data.state) : [];

  // Unfolded rows stay unfolded after a read only while their path exists.
  useEffect(() => {
    setUnfolded((old) => {
      const paths = new Set(rows.map((row) => row.path));
      const kept = new Set([...old].filter((path) => paths.has(path)));
      return kept.size === old.size ? old : kept;
    });
  }, [data]);

  const toggle = (path) => setUnfolded((old) => {
    const next = new Set(old);
    if (!next.delete(path)) next.add(path);
    return next;
  });

  return html`<div class="state-tab">
    <div class="state-head">
      <button onClick=${() => setRefreshes((count) => count + 1)} disabled=${loading}>Odśwież</button>
      ${loading && data && html`<span class="muted small-text">odczytuję…</span>`}
      ${data?.status === 'available' && html`<span class="muted small-text">odczyt ${clock(data.read_at)}</span>`}
    </div>
    ${error && html`<p class="tab-note role-attention">${error === RUNNER_SILENT ? RUNNER_SILENT_HERE : error}</p>`}
    ${!data && !error && loading && html`<p class="empty">odczytuję stan…</p>`}
    ${data?.status === 'unavailable' && html`<p class="tab-note muted">stan niedostępny: ${data.message || data.reason}</p>`}
    ${rows.map((row) => html`<${Row} key=${row.path} row=${row} open=${unfolded.has(row.path)} onToggle=${() => toggle(row.path)} />`)}
  </div>`;
}
