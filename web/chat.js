// The chat of the user with triagent.

import { html, useState, useEffect, useRef, useMemo } from '/web/vendor/preact.js';
import { api, closed, short, clock, useStored, MODELS, ModelSelect } from '/web/app.js';
import { markdown } from '/web/markdown.js';
import { ChangeText, showChange, changeLabel } from '/web/views.js';

// The changes that triagent made while it wrote one message.
function MessageChanges({ ctx, message }) {
  const [changes, setChanges] = useState(null);
  useEffect(() => {
    api
      .ui(`/changes?after=${message.changes_from}&to=${message.changes_to}&actor=triagent`)
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
  const __html = useMemo(() => markdown(message.text, { look: ctx.look }), [message.text, ctx.look]);
  if (message.role === 'user') return html`<div class="msg user md" dangerouslySetInnerHTML=${{ __html }}></div>`;
  return html`
    <div class="msg agent">
      <div class="md" dangerouslySetInnerHTML=${{ __html }}></div>
      ${!message.done && html`<div class="activity">${message.activity || (message.text ? '…' : 'Triagent reads the message…')}</div>`}
      ${message.done && message.changes_to > message.changes_from && html`<${MessageChanges} ctx=${ctx} message=${message} />`}
    </div>
  `;
}

// How well the letters of `needle` match `text`, in the order of the
// needle. A larger number is a better match. -1 is no match. A letter at
// the start of the text or after a separator counts most, so `7bf1` finds
// the id `7b6d7bd0-f1`.
function fuzzy(needle, text) {
  let score = 0;
  let at = 0;
  let last = -2;
  for (const letter of needle) {
    // Prefer the next start of a part of the text to the next letter.
    let found = -1;
    for (let i = at; i < text.length; i++) {
      if (text[i] !== letter) continue;
      const start = i === 0 || '-_.:/ '.includes(text[i - 1]);
      if (i === last + 1 || start) {
        found = i;
        break;
      }
      if (found < 0) found = i;
    }
    if (found < 0) return -1;
    const start = found === 0 || '-_.:/ '.includes(text[found - 1]);
    score += found === last + 1 ? 8 : start ? 6 : 1;
    score -= Math.min(3, found - at) * 0.5;
    last = found;
    at = found + 1;
  }
  return score - text.length * 0.01;
}

// The options of the `@` completion for the text `word` after the `@`, the
// best match first. The id of an option matches by its letters in order.
// The label matches by a part of its text.
function options(ctx, word) {
  const { state, index } = ctx;
  const [kind, rest] = word.includes(':') ? word.split(/:(.*)/) : [null, word];
  const needle = (rest || '').toLowerCase();
  const want = (k) => !kind || k.startsWith(kind);
  const out = [];
  // `order` keeps the order of the list for two equal scores.
  const add = (option, id, ...labels) => {
    let score = needle ? fuzzy(needle, id.toLowerCase()) : 0;
    if (needle && labels.some((l) => l && l.toLowerCase().includes(needle))) score = Math.max(score, needle.length * 4);
    if (score >= 0) out.push({ ...option, score: score - (option.dim ? 2 : 0), order: out.length });
  };
  if (want('item')) {
    for (const i of state.items) add({ kind: 'item', id: i.id, label: i.text, dim: closed(i) }, i.id, i.text);
  }
  if (want('session')) {
    const sessions = [...state.sessions].sort((a, b) => (b.last_ts || '').localeCompare(a.last_ts || ''));
    for (const s of sessions) {
      const topic = s.topic ? index.topics.get(s.topic) : null;
      add({ kind: 'session', id: short(s.chain), label: s.title || short(s.chain) }, short(s.chain), s.title, topic && topic.title);
    }
  }
  if (want('topic')) {
    for (const t of state.topics) add({ kind: 'topic', id: t.id, label: t.title, style: t.style }, t.id, t.title);
  }
  // Without a text, show some options of each kind.
  if (!needle && !kind) {
    const per = (k, n) => out.filter((o) => o.kind === k).slice(0, n);
    return [...per('item', 6), ...per('session', 4), ...per('topic', 4)];
  }
  return out.sort((a, b) => b.score - a.score || a.order - b.order).slice(0, 14);
}

export function Chat({ ctx, handle, events }) {
  const [threads, setThreads] = useState([]);
  const [threadId, setThreadId] = useStored('chat-thread', null);
  const [messages, setMessages] = useState([]);
  const [draft, setDraft] = useStored('chat-draft', '');
  const [model, setModel] = useStored('chat-model', MODELS[0]);
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

  // The events of the reply that triagent writes.
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
    open: (id) => setThreadId(id),
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
      .ui('/chat/messages', { thread: threadId, text, model })
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
          Ask triagent about the items, the sessions and the topics. It can read the sessions and change the triage data.
          <br /><br />Type <kbd>@</kbd> to name an item, a session or a topic. The <b>Ask triagent</b> buttons add a reference here.
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
              <span class=${`k ${o.kind === 'topic' ? 'tc' : ''}`} style=${o.kind === 'topic' ? o.style : ''}>${o.kind}</span>
              <span class="l" style=${o.dim ? 'color:var(--faint)' : ''}>${o.label}</span>
              <span class="i">${o.id}</span>
            </div>`,
          )}
        </div>`}
        <textarea ref=${input} value=${draft} placeholder="Message triagent. @ names an item, a session or a topic."
          onInput=${onInput} onKeyDown=${onKeyDown} onBlur=${() => setComplete(null)}></textarea>
        <div class="bar">
          <span><kbd>Enter</kbd> send · <kbd>Shift</kbd>+<kbd>Enter</kbd> new line</span>
          <span class="spacer"></span>
          <${ModelSelect} value=${model} set=${setModel} title="The model that writes the reply" />
          <button class="primary" onClick=${send} disabled=${busy || !draft.trim()}>${busy ? 'Triagent writes…' : 'Send'}</button>
        </div>
      </div>
    </div>
  `;
}
