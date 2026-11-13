// *Rozmowa*: the conversation of every recorded attempt, or of the attempts
// of one step (*Przebieg* uses the same component). Pages are read with
// `from` / `next_entry`; a closed attempt is not read again. All agent text
// is drawn as text nodes, never as HTML.

import { html, useEffect, useRef, useState } from '../../lib.js';
import { attemptsOf, clock, groupEntries, pagingDone } from '../../text.js';
import { useRead } from '../read.js';

const BYTE_BUDGET = 65_536;
const MAX_PAGES = 40;

/** Conversations already read, by `task/attempt`: `{entries, next, closed}`. */
const cache = new Map();

async function readAttempt(api, taskId, attemptId, signal) {
  const key = `${taskId}/${attemptId}`;
  const known = cache.get(key) ?? { entries: [], next: 0, closed: false, noRecord: false };
  if (known.closed || known.noRecord) return known;
  let { entries, next } = known;
  let closed = false;
  for (let page = 0; page < MAX_PAGES; page += 1) {
    const from = next;
    const answer = await api.conversation(taskId, attemptId, from, signal, BYTE_BUDGET);
    if (answer?.status === 'no_record') {
      const result = { entries: [], next: 0, closed: true, noRecord: true };
      cache.set(key, result);
      return result;
    }
    entries = entries.concat(answer.entries ?? []);
    next = Math.max(next, answer.next_entry ?? next);
    closed = Boolean(answer.closed);
    if (pagingDone(answer, from)) break;
  }
  const result = { entries, next, closed, noRecord: false };
  cache.set(key, result);
  return result;
}

/** Reads the conversations of `positions` (all recorded attempts, in order). */
export async function readConversations(api, taskId, positions, signal) {
  const parts = [];
  for (const position of positions) {
    for (const attemptId of attemptsOf(position)) {
      const attempt = await readAttempt(api, taskId, attemptId, signal);
      parts.push({ position, attemptId, ...attempt });
    }
  }
  return parts;
}

function Fold({ summary, children, open = false, tone = '' }) {
  return html`<details class=${`fold ${tone}`} open=${open}><summary>${summary}</summary>${children}</details>`;
}

function Pre({ text }) {
  return html`<pre class="msg-text">${text}</pre>`;
}

const AUTHOR = { engine: 'narsor', engine_user: 'narsor', conductor: 'dyrygent ⚡', user: 'Ty' };

function Element({ element }) {
  switch (element.kind) {
    case 'prompt':
      return html`<div class="msg msg-prompt">
        <div class="msg-head">${AUTHOR[element.author] ?? 'prompt'} · ${element.purpose ?? 'prompt'}</div>
        <${Fold} summary=${element.text?.split('\n')[0]?.slice(0, 140) || 'prompt'}><${Pre} text=${element.text} /></${Fold}>
      </div>`;
    case 'user_message':
      return html`<div class="msg msg-user"><div class="msg-head">Ty</div><${Pre} text=${element.text} /></div>`;
    case 'agent_text':
      return html`<div class="msg msg-agent"><div class="msg-head">agent ✨</div><${Pre} text=${element.text} /></div>`;
    case 'thinking':
      return html`<${Fold} tone="muted" summary=${element.redacted ? 'myślenie (ukryte)' : 'myślenie…'}><${Pre} text=${element.text ?? ''} /></${Fold}>`;
    case 'agent_question':
      return html`<div class="msg msg-question">
        <div class="msg-head role-attention">pytanie agenta</div>
        <${Pre} text=${element.question} />
        ${element.options?.length > 0 && html`<ul class="options">${element.options.map((option) => html`<li>${option}</li>`)}</ul>`}
      </div>`;
    case 'tool_call':
      return html`<${Fold} tone="tool" summary=${html`<span class="tool-name">${element.tool}</span> <span class="muted">${element.summary ?? ''}</span>`}>
        <${Pre} text=${JSON.stringify(element.input ?? {}, null, 2)} />
      </${Fold}>`;
    case 'tool_result': {
      const error = element.status === 'error';
      return html`<${Fold} tone=${error ? 'tool error' : 'tool'} open=${error}
        summary=${html`<span class=${error ? 'role-problem' : 'role-done'}>${error ? '✗' : '↳'}</span> <span class="tool-name">${element.tool ?? ''}</span> <span class="muted">${element.call_summary ?? ''}</span>`}>
        <${Pre} text=${`${element.content ?? ''}${element.truncated ? '\n…' : ''}`} />
      </${Fold}>`;
    }
    case 'run_result':
      return html`<div class=${`msg msg-result${element.is_error ? ' error' : ''}`}>
        <div class="msg-head">${element.is_error ? html`<span class="role-problem">wynik: błąd</span>` : 'wynik'}
          ${element.turns ? ` · ${element.turns} tur` : ''}${typeof element.cost_usd === 'number' ? ` · $${element.cost_usd.toFixed(2)}` : ''}</div>
        ${element.text && html`<${Pre} text=${element.text} />`}
        ${element.structured_output && html`<${Fold} summary="dane wyjściowe"><${Pre} text=${JSON.stringify(element.structured_output, null, 2)} /></${Fold}>`}
      </div>`;
    case 'session_started':
      return html`<div class="msg-tech muted">sesja ${element.session_id ?? ''}${element.model ? ` · ${element.model}` : ''}</div>`;
    case 'technical':
      return html`<${Fold} tone="muted" summary=${`${element.category}: ${element.summary ?? ''}`}>
        ${element.data !== undefined && html`<${Pre} text=${JSON.stringify(element.data, null, 2)} />`}
      </${Fold}>`;
    default:
      return html`<div class="msg-tech muted">wpis nierozpoznany${element.reason ? ` (${element.reason})` : ''}</div>`;
  }
}

function Entry({ entry }) {
  const [raw, setRaw] = useState(false);
  let rawText = '';
  if (raw) {
    try {
      rawText = new TextDecoder().decode(Uint8Array.from(atob(entry.raw_base64 ?? ''), (c) => c.charCodeAt(0)));
    } catch {
      rawText = '(nie da się odczytać)';
    }
  }
  return html`<div class="entry">
    <div class="entry-meta muted">
      <span>#${entry.entry_number}</span> <span>${clock(entry.timestamp)}</span>
      ${entry.raw_base64 && html`<button class="link" onClick=${() => setRaw(!raw)}>${raw ? 'ukryj surowy' : 'surowy'}</button>`}
    </div>
    ${(entry.elements ?? []).map((element) => html`<${Element} element=${element} />`)}
    ${raw && html`<${Pre} text=${rawText} />`}
  </div>`;
}

const PAGE = 200;

/** The last entries of an attempt; earlier ones on demand, so a long conversation stays fast. */
function Items({ entries }) {
  const [shown, setShown] = useState(PAGE);
  const items = groupEntries(entries);
  const hidden = Math.max(0, items.length - shown);
  return html`${hidden > 0 && html`<button class="small more" onClick=${() => setShown(shown + PAGE)}>pokaż wcześniejsze (${hidden})</button>`}
    <${ItemList} items=${items.slice(hidden)} />`;
}

function ItemList({ items }) {
  return items.map((item) => (item.subagent
    ? html`<${Fold} tone="subagent" summary=${html`◈ subagent <strong>${item.subagent.name}</strong> <span class="muted">· ${item.entries.length} wpisów</span>${item.errors ? html` <span class="role-problem">· ${item.errors} błędów</span>` : ''}`}>
        ${item.entries.map((entry) => html`<${Entry} key=${entry.entry_number} entry=${entry} />`)}
      </${Fold}>`
    : html`<${Entry} key=${item.entry.entry_number} entry=${item.entry} />`));
}

/** The conversation of `positions`; reads again when `revision` changes. */
export function Conversation({ api, task, positions, emptyText = 'brak zapisanej rozmowy' }) {
  const recorded = positions.filter((position) => attemptsOf(position).length > 0);
  const key = `${task.task_id}:${task.revision}:${recorded.map((p) => attemptsOf(p).join(',')).join('|')}`;
  const { data, error, loading } = useRead((signal) => readConversations(api, task.task_id, recorded, signal), key);
  const bottom = useRef(null);
  const box = useRef(null);
  const [follow, setFollow] = useState(true);
  const count = (data ?? []).reduce((sum, part) => sum + part.entries.length, 0);
  useEffect(() => {
    if (follow) bottom.current?.scrollIntoView({ block: 'end' });
  }, [count, follow]);
  const onScroll = (event) => {
    const el = event.currentTarget;
    setFollow(el.scrollHeight - el.scrollTop - el.clientHeight < 40);
  };
  if (!recorded.length) return html`<p class="empty">${emptyText}</p>`;
  return html`<div class="conversation" ref=${box} onScroll=${onScroll}>
    ${error && html`<p class="tab-note role-attention">${error}</p>`}
    ${data === null && loading && html`<p class="empty">wczytuję rozmowę…</p>`}
    ${(data ?? []).map((part) => html`
      <section class="attempt" key=${part.attemptId}>
        <h3 class="attempt-head">${part.position.label} <span class="muted">· próba ${part.attemptId.slice(0, 8)}${part.closed ? '' : ' · trwa'}</span></h3>
        ${part.noRecord ? html`<p class="muted">brak zapisu</p>` : html`<${Items} entries=${part.entries} />`}
      </section>`)}
    <div ref=${bottom}></div>
    ${!follow && html`<button class="follow small" onClick=${() => setFollow(true)}>↧ do końca</button>`}
  </div>`;
}
