// The chat of the user with the triage agent.

import { html, useState, useEffect, useRef, useMemo } from '/web/vendor/preact.js';
import { api, closed, short, clock, useStored } from '/web/app.js';
import { markdown } from '/web/markdown.js';
import { ChangeText, showChange, changeLabel } from '/web/views.js';

// The changes that the agent made while it wrote one message.
function MessageChanges({ ctx, message }) {
  const [changes, setChanges] = useState(null);
  useEffect(() => {
    api
      .ui(`/changes?after=${message.changes_from}&to=${message.changes_to}&actor=agent`)
      .then(setChanges, () => setChanges([]));
  }, [message.id, message.changes_to]);
  if (!changes || changes.length === 0) return null;
  return html`
    <details class="changes">
      <summary>${changes.length} ${changes.length === 1 ? 'change' : 'changes'} of the triage data</summary>
      ${changes.map(
        (c) => html`<div class="c" key=${c.seq}>
          <a href="#" onClick=${(e) => (e.preventDefault(), showChange(ctx, c))}>${changeLabel(ctx, c)}</a>${' '}
          <${ChangeText} change=${c} />
        </div>`,
      )}
    </details>
  `;
}

function Message({ ctx, message }) {
  const __html = useMemo(() => markdown(message.text), [message.text]);
  if (message.role === 'user') return html`<div class="msg user md" dangerouslySetInnerHTML=${{ __html }}></div>`;
  return html`
    <div class="msg agent">
      <div class="md" dangerouslySetInnerHTML=${{ __html }}></div>
      ${!message.done && html`<div class="activity">${message.activity || (message.text ? '…' : 'The agent reads the message…')}</div>`}
      ${message.done && message.changes_to > message.changes_from && html`<${MessageChanges} ctx=${ctx} message=${message} />`}
    </div>
  `;
}

// The options of the `@` completion for the text `word` after the `@`.
function options(ctx, word) {
  const { state, index } = ctx;
  const [kind, rest] = word.includes(':') ? word.split(/:(.*)/) : [null, word];
  const needle = (rest || '').toLowerCase();
  const has = (...texts) => texts.some((t) => t && t.toLowerCase().includes(needle));
  const out = [];
  const want = (k) => !kind || k.startsWith(kind);
  if (want('topic')) {
    for (const t of state.topics) if (has(t.id, t.title)) out.push({ kind: 'topic', id: t.id, label: t.title });
  }
  if (want('item')) {
    for (const i of state.items) {
      if (out.length > 40) break;
      if (has(i.id, i.text)) out.push({ kind: 'item', id: i.id, label: i.text, dim: closed(i) });
    }
  }
  if (want('session')) {
    const sessions = [...state.sessions].sort((a, b) => (b.last_ts || '').localeCompare(a.last_ts || ''));
    for (const s of sessions) {
      const topic = s.topic ? index.topics.get(s.topic) : null;
      if (has(s.chain, s.title, topic && topic.title)) out.push({ kind: 'session', id: short(s.chain), label: s.title || short(s.chain) });
    }
  }
  // Without a kind, show some options of each kind.
  if (!kind) {
    const per = (k, n) => out.filter((o) => o.kind === k).slice(0, n);
    return [...per('item', 6), ...per('session', 4), ...per('topic', 4)];
  }
  return out.slice(0, 14);
}

export function Chat({ ctx, handle, events }) {
  const [threads, setThreads] = useState([]);
  const [threadId, setThreadId] = useStored('chat-thread', null);
  const [messages, setMessages] = useState([]);
  const [draft, setDraft] = useStored('chat-draft', '');
  const [sending, setSending] = useState(false);
  // The `@` completion: `{start, word, at}`.
  const [complete, setComplete] = useState(null);
  const input = useRef(null);
  const list = useRef(null);
  const current = useRef(threadId);
  current.current = threadId;

  const loadThreads = () => api.ui('/chat/threads').then(setThreads, () => {});
  useEffect(() => {
    loadThreads();
  }, []);

  useEffect(() => {
    if (threadId === null) return setMessages([]);
    let live = true;
    api.ui(`/chat/threads/${threadId}`).then(
      (thread) => live && setMessages(thread.messages),
      // The thread does not exist in this server.
      () => live && setThreadId(null),
    );
    return () => (live = false);
  }, [threadId]);

  // The events of the reply that the agent writes.
  useEffect(() => {
    const onMessage = (message) => {
      if (message.thread !== current.current) return;
      setMessages((old) => {
        const at = old.findIndex((m) => m.id === message.id);
        if (at < 0) return [...old, message];
        const next = old.slice();
        next[at] = message;
        return next;
      });
    };
    events.current.add(onMessage);
    return () => events.current.delete(onMessage);
  }, []);

  // Keep the newest text in view, unless the user moved up.
  const nearEnd = useRef(true);
  useEffect(() => {
    const el = list.current;
    if (el && nearEnd.current) el.scrollTop = el.scrollHeight;
  }, [messages]);

  const setText = (text, caret) => {
    setDraft(text);
    if (caret !== undefined) {
      setTimeout(() => {
        const el = input.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(caret, caret);
        grow();
      }, 0);
    }
  };
  const grow = () => {
    const el = input.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(220, el.scrollHeight + 2)}px`;
  };
  useEffect(grow, [draft]);

  handle.current = {
    focus: () => input.current && input.current.focus(),
    add: (reference) => {
      const el = input.current;
      const text = el ? el.value : draft;
      const gap = text && !/\s$/.test(text) ? ' ' : '';
      const next = `${text}${gap}${reference} `;
      setText(next, next.length);
    },
  };

  const busy = sending || messages.some((m) => m.role === 'agent' && !m.done);
  const send = () => {
    const text = draft.trim();
    if (!text || busy) return;
    setSending(true);
    nearEnd.current = true;
    api
      .ui('/chat/messages', { thread: threadId, text })
      .then(
        (thread) => {
          current.current = thread.id;
          setThreadId(thread.id);
          setMessages(thread.messages);
          setDraft('');
          loadThreads();
        },
        (e) => ctx.say(e.message, true),
      )
      .finally(() => setSending(false));
  };

  const choices = complete ? options(ctx, complete.word) : [];
  const choose = (option) => {
    const el = input.current;
    const end = el.selectionStart;
    const reference = `@${option.kind}:${option.id} `;
    const next = draft.slice(0, complete.start) + reference + draft.slice(end);
    setComplete(null);
    setText(next, complete.start + reference.length);
  };
  const onInput = (event) => {
    const el = event.target;
    setDraft(el.value);
    const before = el.value.slice(0, el.selectionStart);
    const match = before.match(/(^|\s)@([A-Za-z0-9._:-]*)$/);
    setComplete(match ? { start: before.length - match[2].length - 1, word: match[2], at: 0 } : null);
  };
  const onKeyDown = (event) => {
    if (complete && choices.length) {
      const move = (by) => setComplete({ ...complete, at: (complete.at + by + choices.length) % choices.length });
      if (event.key === 'ArrowDown') return event.preventDefault(), move(1);
      if (event.key === 'ArrowUp') return event.preventDefault(), move(-1);
      if (event.key === 'Enter' || event.key === 'Tab') return event.preventDefault(), choose(choices[Math.min(complete.at, choices.length - 1)]);
      if (event.key === 'Escape') return event.stopPropagation(), setComplete(null);
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      send();
    }
  };

  return html`
    <div class="chat" onClick=${ctx.onRef}>
      <header>
        <select value=${threadId === null ? '' : String(threadId)} onChange=${(e) => setThreadId(e.target.value === '' ? null : Number(e.target.value))}>
          <option value="">New conversation</option>
          ${threads.map((t) => html`<option value=${String(t.id)}>${clock(t.created)} · ${t.title}</option>`)}
        </select>
        <button onClick=${() => (setThreadId(null), input.current && input.current.focus())} disabled=${threadId === null}>New</button>
      </header>
      <div class="msgs" ref=${list}
        onScroll=${(e) => (nearEnd.current = e.target.scrollHeight - e.target.scrollTop - e.target.clientHeight < 60)}>
        ${messages.length === 0 &&
        html`<div class="empty">
          Ask the triage agent about the items, the sessions and the topics. It can read the sessions and change the triage data.
          <br /><br />Type <kbd>@</kbd> to name an item, a session or a topic. The <b>Ask triage</b> buttons add a reference here.
        </div>`}
        ${messages.map((m) => html`<${Message} key=${m.id} ctx=${ctx} message=${m} />`)}
      </div>
      <div class="composer">
        ${complete &&
        choices.length > 0 &&
        html`<div class="complete">
          ${choices.map(
            (o, at) => html`<div class=${`o ${at === Math.min(complete.at, choices.length - 1) ? 'on' : ''}`}
              onMouseDown=${(e) => (e.preventDefault(), choose(o))}>
              <span class="k">${o.kind}</span>
              <span class="l" style=${o.dim ? 'color:var(--faint)' : ''}>${o.label}</span>
              <span class="i">${o.id}</span>
            </div>`,
          )}
        </div>`}
        <textarea ref=${input} value=${draft} placeholder="Message the triage agent. @ names an item, a session or a topic."
          onInput=${onInput} onKeyDown=${onKeyDown} onBlur=${() => setComplete(null)}></textarea>
        <div class="bar">
          <span><kbd>Enter</kbd> send · <kbd>Shift</kbd>+<kbd>Enter</kbd> new line</span>
          <span class="spacer"></span>
          <button class="primary" onClick=${send} disabled=${busy || !draft.trim()}>${busy ? 'The agent writes…' : 'Send'}</button>
        </div>
      </div>
    </div>
  `;
}
