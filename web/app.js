// The triage web UI: the data of the server, the live events, and the page
// frame. The views are in views.js and the chat is in chat.js.

import { html, render, useState, useEffect, useRef, useCallback, useMemo } from '/web/vendor/preact.js';
import { NowView, TopicsView, ChecksView, ActivityView, SessionPanel, RefPopover } from '/web/views.js';
import { Chat } from '/web/chat.js';
import { lookup, setTopicColors } from '/web/text.js';

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

// ── The address of the page ───────────────────────────────────────────────

// The address holds the navigation state:
//   /<view>          the view (the server has the same list of views)
//   ?q=<text>        the text filter of the Now view
//   ?session=<chain> the session in the session panel, with `&turn=<n>`
//   #<id>            the object that the view shows and marks: an item in
//                    the Now view, a topic in the Topics view, a script in
//                    the Checks view
const VIEWS = [
  ['now', 'Now'],
  ['topics', 'Topics'],
  ['checks', 'Checks'],
  ['activity', 'Activity'],
];

function readRoute() {
  const url = new URL(location.href);
  const view = VIEWS.map(([id]) => id).find((id) => url.pathname === `/${id}`) || 'now';
  let hash = url.hash.slice(1);
  try {
    hash = decodeURIComponent(hash);
  } catch {}
  return {
    view,
    q: url.searchParams.get('q') || '',
    session: url.searchParams.get('session') || null,
    turn: Number(url.searchParams.get('turn')) || null,
    hash,
  };
}

function routeUrl(route) {
  const query = new URLSearchParams();
  if (route.q) query.set('q', route.q);
  if (route.session) query.set('session', route.session);
  if (route.session && route.turn) query.set('turn', String(route.turn));
  const text = query.toString();
  return `/${route.view}${text ? `?${text}` : ''}${route.hash ? `#${encodeURIComponent(route.hash)}` : ''}`;
}

// The route of the page, and the function that changes it. `seq` gets a
// new value at each navigation, but not at a change with `replace`. A view
// shows the object of the hash again when `seq` changes.
function useRoute() {
  const [route, setRoute] = useState(() => ({ ...readRoute(), seq: 0 }));
  const current = useRef(route);
  current.current = route;
  useEffect(() => {
    const onPop = () => setRoute({ ...readRoute(), seq: current.current.seq + 1 });
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  const go = useCallback((change, replace) => {
    const old = current.current;
    const next = { ...old, ...change, seq: replace ? old.seq : old.seq + 1 };
    if (routeUrl(next) !== routeUrl(old)) {
      if (replace) history.replaceState(null, '', routeUrl(next));
      else history.pushState(null, '', routeUrl(next));
    }
    current.current = next;
    setRoute(next);
  }, []);
  return [route, go];
}

// True for a click that the browser must handle: a click that opens a new
// tab or a new window.
const plainClick = (event) => event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey;

// ── The page ──────────────────────────────────────────────────────────────

function App() {
  const [state, setState] = useState(null);
  const [status, setStatus] = useState(null);
  const [error, setError] = useState(null);
  const [route, go] = useRoute();
  const view = route.view;
  const [chatOpen, setChatOpen] = useStored('chat-open', true);
  const [seen, setSeen] = useStored('seen-change', null);
  const [toast, setToast] = useState(null);
  // The reference under the pointer: `{kind, id, rect}`.
  const [hover, setHover] = useState(null);
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
        setState(setTopicColors(s));
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
    // Show an error of the page code. Without this, an error in a render
    // stops the update and the page shows old data.
    const onError = (event) => say(`Page error: ${(event.reason && event.reason.message) || event.message || event.reason}`, true);
    window.addEventListener('error', onError);
    window.addEventListener('unhandledrejection', onError);
    return () => {
      events.close();
      window.removeEventListener('error', onError);
      window.removeEventListener('unhandledrejection', onError);
    };
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
  const look = useMemo(() => (state ? lookup(state) : null), [state]);

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
      look,
      route,
      go,
      say,
      apply,
      setItem: (id, set) => apply([{ op: 'item', id, set }]),
      setSession: (chain, set) => apply([{ op: 'session', chain, set }]),
      openSession: (chain, turn) => go({ session: chain, turn: turn || null }),
      showItem: (id) => go({ view: 'now', q: '', hash: id }),
      showTopic: (id) => go({ view: 'topics', q: '', hash: id }),
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
      // A click on a reference to a triage object.
      onRef: (event) => {
        const a = event.target.closest && event.target.closest('a.ref');
        if (!a || !plainClick(event)) return;
        event.preventDefault();
        setHover(null);
        const { kind, id } = a.dataset;
        if (kind === 'item') ctx.showItem(id);
        else if (kind === 'topic') ctx.showTopic(id);
        else {
          const session = state && state.sessions.find((s) => s.chain.startsWith(id));
          if (session) ctx.openSession(session.chain);
          else say(`No session ${id}`, true);
        }
      },
    };
  }, [state, status, index, look, route]);

  // Show the details of the reference under the pointer.
  const onOver = (event) => {
    const a = event.target.closest && event.target.closest('a.ref');
    if (!a || a.dataset.kind === 'topic') return;
    setHover({ kind: a.dataset.kind, id: a.dataset.id, rect: a.getBoundingClientRect() });
  };
  const onOut = (event) => {
    if (event.target.closest && event.target.closest('a.ref')) setHover(null);
  };

  const openView = (id) => go({ view: id, q: '', hash: '', session: null, turn: null });

  // The keys of the page. The views handle their own keys.
  useEffect(() => {
    const onKey = (event) => {
      const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(event.target.tagName);
      if (event.key === 'Escape') {
        if (route.session) go({ session: null, turn: null });
        else if (typing) event.target.blur();
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === 'c') {
        event.preventDefault();
        setChatOpen(true);
        setTimeout(() => chat.current && chat.current.focus(), 0);
      } else if (VIEWS[Number(event.key) - 1]) openView(VIEWS[Number(event.key) - 1][0]);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [route.session]);

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
    <div class=${`shell ${chatOpen ? 'chat-open' : ''}`} onMouseOver=${onOver} onMouseOut=${onOut}>
      <div class="top">
        <span class="name">recap triage</span>
        <div class="tabs">
          ${VIEWS.map(
            ([id, label]) => html`<a class=${view === id ? 'on' : ''} href=${`/${id}`}
              onClick=${(e) => plainClick(e) && (e.preventDefault(), openView(id))}>
              ${label}${id === 'checks' && unapproved.length > 0 && html`<span class="badge" title="Check scripts that wait for your approval">${unapproved.length}</span>`}
            </a>`,
          )}
        </div>
        <div class="counts">
          ${count('critical') > 0 && html`<span class="pill crit" title="Open critical items">${count('critical')} critical</span>`}
          ${count('high') > 0 && html`<span class="pill high" title="Open high items">${count('high')} high</span>`}
          <span class="pill" title="All open items that are not muted">${visible.length} open</span>
          ${waiting.length > 0 && html`<span class="pill wait" title="Sessions that wait for you now">${waiting.length} waiting</span>`}
          ${fresh > 0 &&
          html`<button class="pill info" onClick=${() => openView('activity')} title="Changes since your last look at the activity">
            ${fresh} ${fresh === 1 ? 'change' : 'changes'}
          </button>`}
        </div>
        <span class="spacer"></span>
        <span class=${`runner ${active ? 'active' : ''}`} title=${run && run.activity ? run.activity : ''}>
          ${active
            ? `Triagent run: ${run.activity || 'started'}`
            : status && status.next_run
              ? `Next triagent run ${until(status.next_run)}`
              : run
                ? `Last triagent run ${ago(run.ended)} ago${run.success ? '' : ' (failed)'}`
                : 'No triagent run yet'}
        </span>
        <button class="plain" onClick=${() => api.post('/v1/triage/review').then(() => say('Asked for a review run'), (e) => say(e.message, true))} title="Ask triagent to read all the sessions of the last 24 hours">
          Review
        </button>
        <button class="plain" onClick=${() => setChatOpen(!chatOpen)} title="Show or hide the chat (c)">${chatOpen ? 'Hide chat' : 'Chat'}</button>
      </div>
      <div class="body">
        <div class="main" onClick=${ctx.onRef}>
          <div class="inner">
            ${view === 'now' && html`<${NowView} ctx=${ctx} />`}
            ${view === 'topics' && html`<${TopicsView} ctx=${ctx} />`}
            ${view === 'checks' && html`<${ChecksView} ctx=${ctx} />`}
            ${view === 'activity' && html`<${ActivityView} ctx=${ctx} seen=${seen} setSeen=${setSeen} />`}
          </div>
        </div>
        ${chatOpen && html`<${Chat} ctx=${ctx} handle=${chat} events=${chatEvents} />`}
      </div>
      ${route.session &&
      html`<${SessionPanel} key=${route.session} ctx=${ctx} chain=${route.session} turn=${route.turn}
        close=${() => go({ session: null, turn: null })} />`}
      ${hover && html`<${RefPopover} ctx=${ctx} hover=${hover} />`}
      ${toast && html`<div class=${`toast ${toast.isError ? 'error' : ''}`}>${toast.text}</div>`}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('app'));
