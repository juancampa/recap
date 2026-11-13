// The triage web UI: the data of the server, the live events, and the page
// frame. The views are in views.js and the chat is in chat.js.

import { html, render, useState, useEffect, useRef, useCallback, useMemo } from '/web/vendor/preact.js';
import { NowView, TopicsView, ChecksView, ActivityView, SessionPanel } from '/web/views.js';
import { Chat } from '/web/chat.js';

const TOKEN = document.querySelector('meta[name="recap-token"]').content;

// ── The server calls ──────────────────────────────────────────────────────

async function request(path, options = {}) {
  const response = await fetch(path, options);
  if (!response.ok) throw new Error((await response.text()) || `${response.status}`);
  if (response.status === 204) return null;
  return response.json();
}

export const api = {
  get: (path) => request(path),
  // A route that only the web UI of the user can call.
  ui: (path, body, method) =>
    request(`/ui${path}`, {
      method: method || (body === undefined ? 'GET' : 'POST'),
      headers: { 'X-Recap-Token': TOKEN, 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
    }),
  post: (path) => request(path, { method: 'POST' }),
};

// ── Helpers that the views share ──────────────────────────────────────────

export const closed = (item) => ['verified', 'confirmed', 'dropped'].includes(item.state);
export const short = (id) => (id || '').slice(0, 8);

export function ago(time) {
  if (!time) return '';
  const s = (Date.now() - new Date(time).getTime()) / 1000;
  if (s < 0) return until(time);
  if (s < 90) return 'now';
  if (s < 5400) return `${Math.round(s / 60)}m`;
  if (s < 129600) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86400)}d`;
}

export function until(time) {
  const s = (new Date(time).getTime() - Date.now()) / 1000;
  if (s <= 0) return `${ago(time)} ago`;
  if (s < 5400) return `in ${Math.round(s / 60)}m`;
  if (s < 129600) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
}

export function clock(time) {
  if (!time) return '';
  const d = new Date(time);
  const day = d.toDateString() === new Date().toDateString() ? '' : `${d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' })} `;
  return `${day}${d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })}`;
}

const stored = (key, fallback) => {
  try {
    const value = localStorage.getItem(key);
    return value === null ? fallback : JSON.parse(value);
  } catch {
    return fallback;
  }
};

// A state value that the browser keeps between two visits.
export function useStored(key, fallback) {
  const [value, setValue] = useState(() => stored(key, fallback));
  const set = useCallback(
    (next) => {
      setValue(next);
      try {
        localStorage.setItem(key, JSON.stringify(next));
      } catch {}
    },
    [key],
  );
  return [value, set];
}

// ── The page ──────────────────────────────────────────────────────────────

function App() {
  const [state, setState] = useState(null);
  const [status, setStatus] = useState(null);
  const [error, setError] = useState(null);
  const [view, setView] = useStored('view', 'now');
  const [chatOpen, setChatOpen] = useStored('chat-open', true);
  const [seen, setSeen] = useStored('seen-change', null);
  // The session panel: `{chain, turn}`.
  const [panel, setPanel] = useState(null);
  // The item that the Now view must show and expand.
  const [focusItem, setFocusItem] = useState(null);
  const [focusTopic, setFocusTopic] = useState(null);
  const [toast, setToast] = useState(null);
  const chat = useRef(null);
  const chatEvents = useRef(new Set());

  const say = useCallback((text, isError) => {
    setToast({ text, isError });
    setTimeout(() => setToast((t) => (t && t.text === text ? null : t)), isError ? 6000 : 1800);
  }, []);

  const loadState = useCallback(() => {
    api
      .get('/v1/triage/state')
      .then((s) => {
        setState(s);
        setError(null);
      })
      .catch((e) => setError(e.message));
  }, []);
  const loadStatus = useCallback(() => {
    api.get('/v1/status').then((s) => setStatus(s.triage)).catch(() => {});
  }, []);

  useEffect(() => {
    loadState();
    loadStatus();
    // One request for many events that come together.
    let timer = null;
    const soon = () => {
      clearTimeout(timer);
      timer = setTimeout(loadState, 150);
    };
    const events = new EventSource('/v1/events');
    events.onmessage = (message) => {
      const event = JSON.parse(message.data);
      if (event.type === 'triage_data' || event.type === 'running' || event.type === 'changed') soon();
      else if (event.type === 'triage') setStatus(event);
      else if (event.type === 'triage_chat') chatEvents.current.forEach((f) => f(event.message));
    };
    // The browser connects again after an error. The data can be old then.
    events.onopen = () => {
      loadState();
      loadStatus();
    };
    return () => events.close();
  }, []);

  // The first visit has no changes to show as new.
  useEffect(() => {
    if (state && seen === null) setSeen(state.last_change);
  }, [state && state.last_change]);

  const index = useMemo(() => {
    const sessions = new Map();
    const topics = new Map();
    const items = new Map();
    if (state) {
      state.sessions.forEach((s) => sessions.set(s.chain, s));
      state.topics.forEach((t) => topics.set(t.id, t));
      state.items.forEach((i) => items.set(i.id, i));
    }
    return { sessions, topics, items };
  }, [state]);

  const ctx = useMemo(() => {
    const apply = (ops) =>
      api
        .ui('/apply', { ops })
        .then((result) => {
          const failed = result.errors.find((e) => e);
          if (failed) say(failed, true);
          loadState();
          return !failed;
        })
        .catch((e) => {
          say(e.message, true);
          return false;
        });
    return {
      state,
      status,
      index,
      say,
      apply,
      setItem: (id, set) => apply([{ op: 'item', id, set }]),
      setSession: (chain, set) => apply([{ op: 'session', chain, set }]),
      openSession: (chain, turn) => setPanel({ chain, turn }),
      showItem: (id) => {
        setView('now');
        setFocusItem({ id, at: Date.now() });
      },
      showTopic: (id) => {
        setView('topics');
        setFocusTopic({ id, at: Date.now() });
      },
      // Add a reference to the chat draft and move the focus to the chat.
      ask: (reference) => {
        setChatOpen(true);
        setTimeout(() => chat.current && chat.current.add(reference), 0);
      },
      goToSession: (chain) =>
        api
          .ui(`/sessions/${chain}/focus`, {})
          .then(() => say('Opened the session window'))
          .catch((e) => say(e.message, true)),
      copy: (text) =>
        navigator.clipboard.writeText(text).then(
          () => say('Copied'),
          () => say('The browser refused the copy', true),
        ),
      // A click on a reference in rendered Markdown.
      onRef: (event) => {
        const a = event.target.closest && event.target.closest('a.ref');
        if (!a) return;
        event.preventDefault();
        const { kind, id } = a.dataset;
        if (kind === 'item') ctx.showItem(id);
        else if (kind === 'topic') ctx.showTopic(id);
        else {
          const session = state && state.sessions.find((s) => s.chain.startsWith(id));
          if (session) setPanel({ chain: session.chain });
          else say(`No session ${id}`, true);
        }
      },
    };
  }, [state, status, index]);

  // The keys of the page. The views handle their own keys.
  useEffect(() => {
    const onKey = (event) => {
      const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(event.target.tagName);
      if (event.key === 'Escape') {
        if (panel) setPanel(null);
        else if (typing) event.target.blur();
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === 'c') {
        event.preventDefault();
        setChatOpen(true);
        setTimeout(() => chat.current && chat.current.focus(), 0);
      } else if (event.key === '1') setView('now');
      else if (event.key === '2') setView('topics');
      else if (event.key === '3') setView('checks');
      else if (event.key === '4') setView('activity');
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [panel]);

  if (error && !state) {
    return html`<div class="note" style="padding:40px">
      The server has no triage data: ${error}
      <br />Start the server with <code>--triage-dir</code>.
    </div>`;
  }
  if (!state) return html`<div class="note" style="padding:40px">Loading…</div>`;

  const open = state.items.filter((i) => !closed(i));
  const visible = open.filter((i) => {
    const session = index.sessions.get(i.chain);
    return !(session && session.muted) || i.impact === 'critical';
  });
  const count = (impact) => visible.filter((i) => i.impact === impact).length;
  const waiting = state.sessions.filter((s) => s.status === 'waiting');
  const unapproved = (status && status.unapproved) || [];
  const fresh = seen === null ? 0 : Math.max(0, state.last_change - seen);
  const run = status && status.run;
  const active = run && !run.ended;

  return html`
    <div class=${`shell ${chatOpen ? "chat-open" : ""}`}>
      <div class="top">
        <span class="name">recap triage</span>
        <div class="tabs">
          ${[
            ['now', 'Now'],
            ['topics', 'Topics'],
            ['checks', 'Checks'],
            ['activity', 'Activity'],
          ].map(([id, label]) => html`<button class=${view === id ? 'on' : ''} onClick=${() => setView(id)}>${label}</button>`)}
        </div>
        <div class="counts">
          ${count('critical') > 0 && html`<span class="pill crit" title="Open critical items">${count('critical')} critical</span>`}
          ${count('high') > 0 && html`<span class="pill high" title="Open high items">${count('high')} high</span>`}
          <span class="pill" title="All open items that are not muted">${visible.length} open</span>
          ${waiting.length > 0 && html`<span class="pill wait" title="Sessions that wait for you now">${waiting.length} waiting</span>`}
          ${unapproved.length > 0 &&
          html`<button class="pill info" onClick=${() => setView('checks')} title="Check scripts that wait for your approval">
            ${unapproved.length} to approve
          </button>`}
          ${fresh > 0 &&
          html`<button class="pill info" onClick=${() => setView('activity')} title="Changes since your last look at the activity">
            ${fresh} ${fresh === 1 ? 'change' : 'changes'}
          </button>`}
        </div>
        <span class="spacer"></span>
        <span class=${`runner ${active ? 'active' : ''}`} title=${run && run.activity ? run.activity : ''}>
          ${active
            ? `Triage run: ${run.activity || 'started'}`
            : status && status.next_run
              ? `Next triage run ${until(status.next_run)}`
              : run
                ? `Last triage run ${ago(run.ended)} ago${run.success ? '' : ' (failed)'}`
                : 'No triage run yet'}
        </span>
        <button class="plain" onClick=${() => api.post('/v1/triage/review').then(() => say('Asked for a review run'), (e) => say(e.message, true))} title="Ask the triage agent to read all the sessions of the last 24 hours">
          Review
        </button>
        <button class="plain" onClick=${() => setChatOpen(!chatOpen)} title="Show or hide the chat (c)">${chatOpen ? 'Hide chat' : 'Chat'}</button>
      </div>
      <div class="body">
        <div class="main" onClick=${ctx.onRef}>
          <div class="inner">
            ${view === 'now' && html`<${NowView} ctx=${ctx} focus=${focusItem} />`}
            ${view === 'topics' && html`<${TopicsView} ctx=${ctx} focus=${focusTopic} />`}
            ${view === 'checks' && html`<${ChecksView} ctx=${ctx} />`}
            ${view === 'activity' && html`<${ActivityView} ctx=${ctx} seen=${seen} setSeen=${setSeen} />`}
          </div>
        </div>
        ${chatOpen && html`<${Chat} ctx=${ctx} handle=${chat} events=${chatEvents} />`}
      </div>
      ${panel && html`<${SessionPanel} ctx=${ctx} chain=${panel.chain} turn=${panel.turn} close=${() => setPanel(null)} />`}
      ${toast && html`<div class=${`toast ${toast.isError ? 'error' : ''}`}>${toast.text}</div>`}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('app'));
