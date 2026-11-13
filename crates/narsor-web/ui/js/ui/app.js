// The shell: header, task list and details, the notification and the help.
// Keyboard shortcuts follow the TUI where they make sense in a browser;
// they do nothing while the focus is in a text field.

import { html, useEffect, useRef, useState } from '../lib.js';
import { titleOf } from '../model.js';
import { ActionPanel } from './actions.js';
import { hasUnsentDraft } from './answer.js';
import { Details, TABS } from './details.js';
import { Header } from './header.js';
import { NewTask } from './newtask.js';
import { ArchivedTask, Harness, History, Limit, Repositories } from './settings.js';
import { Help } from './help.js';
import { useNow, useStore } from './hooks.js';
import { TaskList } from './tasklist.js';
import { Toast } from './toast.js';

function typing(target) {
  return target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));
}

export function App({ store, onSignOut }) {
  const state = useStore(store);
  const now = useNow(1000);
  const [help, setHelp] = useState(false);
  const [detailsOpen, setDetailsOpen] = useState(false);
  const [actionsOpen, setActionsOpen] = useState(false);
  const [newOpen, setNewOpen] = useState(false);
  const [dialog, setDialog] = useState(null);
  const filterRef = useRef(null);
  const rows = store.shown();
  const selected = (state.tasks ?? []).find((task) => task.task_id === state.selectedTaskId) ?? null;

  const move = (task, delta) => {
    const queue = rows.filter((row) => row.runner_state === 'queued' && row.backend_id === task.backend_id);
    const index = queue.indexOf(task);
    const anchor = queue[index + delta];
    if (!anchor) {
      store.notify('warning', `przesuń: to zadanie jest już ${delta < 0 ? 'pierwsze' : 'ostatnie'} w kolejce`);
      return;
    }
    const target = { to: delta < 0 ? 'before' : 'after', task_id: anchor.task_id };
    store.mutate((api) => api.move(task.task_id, target, task.generation), {
      success: `${titleOf(task)}: przesunięto ${delta < 0 ? 'przed' : 'za'} ${titleOf(anchor)}`,
      failure: `${titleOf(task)}: przesunięcie odrzucone`,
    }).catch(() => {});
  };

  // The address mirrors the view (`#/task/<id>/<tab>`), so a reload comes back
  // to the same place. The token is never in it.
  useEffect(() => {
    const [, kind, id, tab] = window.location.hash.split('/');
    if (kind === 'task' && id) {
      store.set({ selectedTaskId: decodeURIComponent(id), ...(TABS.some(([key]) => key === tab) ? { tab } : {}) });
      setDetailsOpen(true);
    }
  }, [store]);
  useEffect(() => {
    if (!state.selectedTaskId) return;
    const hash = `#/task/${encodeURIComponent(state.selectedTaskId)}/${state.tab}`;
    if (window.location.hash !== hash) window.history.replaceState(null, '', hash);
  }, [state.selectedTaskId, state.tab]);

  // Ask before the tab closes while a form has text that was not sent.
  useEffect(() => {
    const onUnload = (event) => {
      if (!hasUnsentDraft()) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', onUnload);
    return () => window.removeEventListener('beforeunload', onUnload);
  }, []);

  const cycleTab = (delta) => {
    const index = TABS.findIndex(([key]) => key === store.state.tab);
    store.set({ tab: TABS[(index + delta + TABS.length) % TABS.length][0] });
  };

  useEffect(() => {
    const onKey = (event) => {
      if (event.ctrlKey || event.metaKey || event.altKey) return;
      if (typing(event.target) || actionsOpen || newOpen || dialog) return;
      if (help && event.key === 'Escape') { setHelp(false); return; }
      const keys = {
        j: () => store.step(1),
        ArrowDown: () => store.step(1),
        k: () => store.step(-1),
        ArrowUp: () => store.step(-1),
        g: () => store.step(-Infinity),
        G: () => store.step(Infinity),
        ' ': () => store.selectNextWaiting(),
        '/': () => filterRef.current?.focus(),
        z: () => store.setView({ hideDone: !store.state.hideDone }),
        Enter: () => setDetailsOpen(true),
        Escape: () => setDetailsOpen(false),
        '?': () => setHelp(true),
        u: () => store.refresh(),
        H: () => setDialog({ kind: 'history' }),
        R: () => setDialog({ kind: 'repositories' }),
        h: () => setDialog({ kind: 'harness' }),
        L: () => setDialog({ kind: 'limit' }),
        n: () => setNewOpen(true),
        '.': () => { if (selected) setActionsOpen(true); },
        '[': () => cycleTab(-1),
        ']': () => cycleTab(1),
        J: () => { const task = rows.find((row) => row.task_id === store.state.selectedTaskId); if (task?.runner_state === 'queued') move(task, 1); },
        K: () => { const task = rows.find((row) => row.task_id === store.state.selectedTaskId); if (task?.runner_state === 'queued') move(task, -1); },
      };
      const action = keys[event.key];
      if (action) {
        event.preventDefault();
        action();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  return html`
    <div class=${`shell${detailsOpen ? ' details-open' : ''}`}>
      <${Header} state=${state} now=${now} onHelp=${() => setHelp(true)} onSignOut=${onSignOut} onDialog=${(kind) => setDialog({ kind })} />
      <main class="panes">
        <${TaskList} store=${store} state=${state} rows=${rows} now=${now} filterRef=${filterRef}
          onOpen=${(id) => { store.select(id); setDetailsOpen(true); }} onMove=${move} onNew=${() => setNewOpen(true)} />
        <${Details} store=${store} state=${state} task=${selected} now=${now} onBack=${() => setDetailsOpen(false)}
          onActions=${() => setActionsOpen(true)} />
      </main>
      <${Toast} notification=${state.notification} />
      ${help && html`<${Help} onClose=${() => setHelp(false)} />`}
      ${newOpen && html`<${NewTask} store=${store} onClose=${() => setNewOpen(false)}
        onCreated=${(id) => { setNewOpen(false); store.select(id); }} />`}
      ${dialog?.kind === 'history' && html`<${History} store=${store} onClose=${() => setDialog(null)} onOpen=${(task) => setDialog({ kind: 'archived', task })} />`}
      ${dialog?.kind === 'archived' && html`<${ArchivedTask} task=${dialog.task} onClose=${() => setDialog({ kind: 'history' })} />`}
      ${dialog?.kind === 'repositories' && html`<${Repositories} store=${store} onClose=${() => setDialog(null)} />`}
      ${dialog?.kind === 'harness' && html`<${Harness} store=${store} onClose=${() => setDialog(null)} />`}
      ${dialog?.kind === 'limit' && html`<${Limit} store=${store} onClose=${() => setDialog(null)} />`}
      ${actionsOpen && selected && html`<${ActionPanel} store=${store} task=${selected} onClose=${() => setActionsOpen(false)}
        onAnswer=${() => document.querySelector('.answer-panel input, .answer-panel textarea')?.focus()} />`}
    </div>`;
}
