// *Dyrygent*: what the conductor does now, its chat, its proposals and its
// decisions. Every action goes through `POST …/operator-commands`.

import { html, useState } from '../../lib.js';
import { clock } from '../../text.js';
import { useRead } from '../read.js';

const PURPOSE = {
  choose_output: 'wybiera wynik kroku',
  choose_output_after_reprompt: 'wybiera wynik po ponownym pytaniu',
  answer_agent_question: 'odpowiada na pytanie agenta',
  diagnose_failure: 'diagnozuje błąd',
  reply_to_chat: 'odpowiada na wiadomość',
  summarize_gate: 'podsumowuje bramkę',
  generate_title: 'nadaje tytuł',
  decide_fork: 'decyduje o rozgałęzieniu',
};

export function Conductor({ store, task }) {
  const api = store.api;
  const { data, error, loading } = useRead((signal) => api.conductor(task.task_id, 0, undefined, signal), `${task.task_id}:${task.revision}`);
  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);

  const command = (body, success) => store.mutate(
    (client) => client.operatorCommand(task.task_id, body, task.generation),
    { success, failure: 'dyrygent: odrzucone' },
  );

  const send = async (event) => {
    event.preventDefault();
    const value = text.trim();
    if (!value) return;
    setSending(true);
    try {
      const result = await command({ kind: 'conductor_chat', text: value }, 'wiadomość wysłana do dyrygenta');
      if (result !== undefined) setText('');
    } catch {
      /* the text stays */
    } finally {
      setSending(false);
    }
  };

  if (data?.status === 'unavailable') {
    return html`<p class="tab-note muted">dyrygent niedostępny: ${data.message || data.reason}</p>`;
  }
  const view = data?.conductor;
  return html`<div class="conductor">
    ${error && html`<p class="tab-note role-attention">${error}</p>`}
    ${!view && loading && html`<p class="empty">wczytuję…</p>`}
    ${view && html`
      <div class="conductor-status">
        <span class="role-calm">⚡ ${view.harness ?? 'dyrygent'}</span>
        ${view.available === false && html`<span class="muted"> · niedostępny: ${view.unavailable_reason ?? ''}</span>`}
        ${(view.working ?? []).filter((w) => w.running).map((w) => html`<span class="role-live"> · ${PURPOSE[w.purpose] ?? w.purpose}…</span>`)}
      </div>

      ${view.failure && html`
        <section class="card">
          <h3 class=${view.failure.outcome?.kind === 'retried' ? 'role-done' : 'role-problem'}>Błąd kroku ${view.failure.state_id}</h3>
          <pre class="msg-text">${view.failure.reason}</pre>
          ${view.failure.outcome?.kind === 'diagnosed' && html`<p><strong>Diagnoza:</strong></p><pre class="msg-text">${view.failure.outcome.diagnosis}</pre>`}
          ${view.failure.outcome?.kind === 'unanswered' && html`<p class="muted">dyrygent nie odpowiedział: ${view.failure.outcome.error}</p>`}
          ${view.failure.outcome?.kind === 'retried' && html`<p class="muted">ponowiono${view.failure.outcome.note ? `: ${view.failure.outcome.note}` : ''}</p>`}
        </section>`}

      ${(view.proposals ?? []).length > 0 && html`
        <section>
          <h3>Propozycje</h3>
          ${view.proposals.map((proposal) => html`
            <div class="card" key=${proposal.proposal_id}>
              <div class="muted small-text">${clock(proposal.at)} · ${proposal.record?.proposal?.kind ?? ''} · ${proposal.record?.confidence ?? ''} · ${proposal.state}</div>
              <pre class="msg-text">${proposal.text}</pre>
              ${proposal.record?.rationale && html`<p class="muted small-text">${proposal.record.rationale}</p>`}
              ${proposal.state === 'pending' && html`
                <div class="buttons">
                  <button class="primary" onClick=${() => command({ kind: 'confirm_proposal', proposal_id: proposal.proposal_id }, 'propozycja potwierdzona').catch(() => {})}>Potwierdź</button>
                  <button onClick=${() => command({ kind: 'reject_proposal', proposal_id: proposal.proposal_id }, 'propozycja odrzucona').catch(() => {})}>Odrzuć</button>
                </div>`}
            </div>`)}
        </section>`}

      ${(view.decisions ?? []).length > 0 && html`
        <section>
          <h3>Decyzje</h3>
          ${view.decisions.map((decision) => html`
            <div class="card" key=${decision.decision_id}>
              <div class="muted small-text">${clock(decision.at)} · ${decision.kind} · ${decision.state_id ?? ''} · ${decision.confidence}${decision.undone ? ' · cofnięta' : ''}</div>
              <pre class="msg-text">${decision.text}</pre>
              ${decision.rationale && html`<p class="muted small-text">${decision.rationale}</p>`}
              ${decision.undoable && !decision.undone && html`
                <button onClick=${() => command({ kind: 'undo_decision', decision_id: decision.decision_id }, 'decyzja cofnięta').catch(() => {})}>Cofnij</button>`}
            </div>`)}
        </section>`}

      <section>
        <h3>Czat</h3>
        <div class="chat">
          ${(view.chat ?? []).length === 0 && html`<p class="muted">brak wiadomości</p>`}
          ${(view.chat ?? []).map((entry) => html`
            <div class=${`msg ${entry.author === 'operator' ? 'msg-user' : 'msg-agent'}${entry.kind === 'error' ? ' error' : ''}`} key=${entry.id}>
              <div class="msg-head">${entry.author === 'operator' ? 'Ty' : 'dyrygent ⚡'} <span class="muted">· ${clock(entry.at)}${entry.kind !== 'message' ? ` · ${entry.kind}` : ''}</span></div>
              <pre class="msg-text">${entry.text}</pre>
            </div>`)}
        </div>
      </section>`}

    <form class="composer" onSubmit=${send}>
      <textarea rows="2" placeholder="Napisz do dyrygenta…" aria-label="Wiadomość do dyrygenta" value=${text}
        onInput=${(event) => setText(event.currentTarget.value)}
        onKeyDown=${(event) => { if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) send(event); }}></textarea>
      <button type="submit" class="primary" disabled=${sending || !text.trim() || view?.available === false}
        title=${view?.available === false ? 'dyrygent jest niedostępny' : 'Ctrl+Enter'}>Wyślij</button>
    </form>
  </div>`;
}
