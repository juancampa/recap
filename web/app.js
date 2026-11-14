// The triage web UI: the data of the server, the live events, and the page
// frame. The views are in views.js and the chat is in chat.js.

import { html, render, useState, useEffect, useRef, useCallback, useMemo } from '/web/vendor/preact.js';
import { ItemsView, ChecksView, ActivityView, SessionPanel, RefPopover } from '/web/views.js';
import { Chat } from '/web/chat.js';
import { lookup, setTopicColors } from '/web/text.js';

const TOKEN = document.querySelector('meta[name="recap-token"]').content;

// ── The server calls ──────────────────────────────────────────────────────

// The page calls `restart.notice` when it finds that the server started
// again. The server then has a new token, and it refuses the `/ui/` calls of
// this page.
const restart = { notice: () => {} };

async function request(path, options = {}) {
  const response = await fetch(path, options);
  if (response.status === 403 && path.startsWith('/ui/')) restart.notice();
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

// The models that triagent can use for a review run and for a chat reply.
// The first one is the model of each other run.
export const MODELS = ['opus', 'fable', 'sonnet', 'haiku'];

// The selector of one of `MODELS`.
export function ModelSelect({ value, set, title }) {
  return html`<select class="model" value=${value} title=${title} onChange=${(e) => set(e.target.value)}>
    ${MODELS.map((m) => html`<option value=${m}>${m}</option>`)}
  </select>`;
}

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

// A state value that the browser keeps for this tab. A reload keeps it.
export function useSession(key, fallback) {
  const [value, setValue] = useState(() => {
    try {
      const text = sessionStorage.getItem(key);
      return text === null ? fallback : JSON.parse(text);
    } catch {
      return fallback;
    }
  });
  const set = useCallback(
    (next) => {
      setValue(next);
      try {
        sessionStorage.setItem(key, JSON.stringify(next));
      } catch {}
    },
    [key],
  );
  return [value, set];
}

// True when the user loaded the page again.
const navigation = performance.getEntriesByType('navigation')[0];
const RELOADED = Boolean(navigation && navigation.type === 'reload');

// ── The address of the page ───────────────────────────────────────────────

// The address holds the navigation state:
//   /<view>          the view (the server has the same list of views)
//   ?q=<text>        the text filter of the Items view
//   ?group=<name>    the grouping of the Items view: `topic` or `session`
//   ?session=<chain> the session in the session panel, with `&turn=<n>`.
//                    `&ref=<item>/<index>` names a reference of an item:
//                    the panel marks the quote of the reference
//   #<id>            the object that the view shows and marks: an item in
//                    the Items view (`#topic:<id>` shows the group of a
//                    topic), and a script in the Checks view
const VIEWS = [
  ['items', 'Items'],
  ['checks', 'Checks'],
  ['activity', 'Activity'],
];

function readRoute() {
  const url = new URL(location.href);
  const view = VIEWS.map(([id]) => id).find((id) => url.pathname === `/${id}`) || 'items';
  let hash = url.hash.slice(1);
  try {
    hash = decodeURIComponent(hash);
  } catch {}
  return {
    view,
    q: url.searchParams.get('q') || '',
    group: ['topic', 'session'].find((g) => g === url.searchParams.get('group')) || '',
    session: url.searchParams.get('session') || null,
    turn: Number(url.searchParams.get('turn')) || null,
    ref: url.searchParams.get('ref') || null,
    hash,
  };
}

function routeUrl(route) {
  const query = new URLSearchParams();
  if (route.q) query.set('q', route.q);
  if (route.view === 'items' && route.group) query.set('group', route.group);
  if (route.session) query.set('session', route.session);
  if (route.session && route.turn) query.set('turn', String(route.turn));
  if (route.session && route.turn && route.ref) query.set('ref', route.ref);
  const text = query.toString();
  return `/${route.view}${text ? `?${text}` : ''}${route.hash ? `#${encodeURIComponent(route.hash)}` : ''}`;
}

// The route of the page, and the function that changes it. `seq` gets a
// new value at each navigation, but not at a change with `replace`. A view
// shows the object of the hash again when `seq` changes.
function useRoute() {
  // `restore` is true after a reload and after a Back or Forward step of
  // the browser. Then the page shows the scroll position that the view had
  // before, and does not scroll to the object of the hash.
  const [route, setRoute] = useState(() => ({ ...readRoute(), seq: 0, restore: RELOADED }));
  const current = useRef(route);
  current.current = route;
  useEffect(() => {
    const onPop = () => setRoute({ ...readRoute(), seq: current.current.seq + 1, restore: true });
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  const go = useCallback((change, replace) => {
    const old = current.current;
    const next = { ...old, ...change, seq: replace ? old.seq : old.seq + 1, restore: replace ? old.restore : false };
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
  const [reviewModel, setReviewModel] = useStored('review-model', MODELS[0]);
  const [seen, setSeen] = useStored('seen-change', null);
  const [toast, setToast] = useState(null);
  // True when the server started again after the load of this page.
  const [restarted, setRestarted] = useState(false);
  restart.notice = () => setRestarted(true);
  const [panelWidth, setPanelWidth] = useStored('session-width', 620);
  // A prompt of an item that the session panel shows in its input:
  // `{chain, text}`.
  const [written, setWritten] = useState(null);
  // The reference under the pointer: `{kind, id, rect}`.
  const [hover, setHover] = useState(null);
  const chat = useRef(null);
  const main = useRef(null);
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
    // The server can be a new process: then its page has a new token.
    let connected = false;
    events.onopen = () => {
      loadState();
      loadStatus();
      if (connected) {
        fetch('/', { cache: 'no-store' })
          .then((r) => r.text())
          .then((page) => {
            const token = page.match(/name="recap-token" content="([^"]*)"/);
            if (token && token[1] !== TOKEN) setRestarted(true);
          })
          .catch(() => {});
      }
      connected = true;
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
      // `ref` names a reference of an item, as `<item>/<index>`.
      openSession: (chain, turn, ref) => go({ session: chain, turn: turn || null, ref: (turn && ref) || null }),
      // Show the session panel with `text` in its prompt input.
      writePrompt: (chain, text) => {
        setWritten({ chain, text });
        go({ session: chain, turn: null, ref: null });
      },
      showItem: (id) => go({ view: 'items', q: '', hash: id }),
      showTopic: (id) => go({ view: 'items', group: 'topic', q: '', hash: `topic:${id}` }),
      // Add a reference to the chat draft and move the focus to the chat.
      ask: (reference) => {
        setChatOpen(true);
        setTimeout(() => chat.current && chat.current.add(reference), 0);
      },
      // Show the chat thread `id`.
      openThread: (id) => {
        setChatOpen(true);
        setTimeout(() => chat.current && chat.current.open(id), 0);
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

  // Each view keeps its scroll position for this tab. A view shows it
  // again, unless the address names an object to show.
  const loaded = Boolean(state);
  useEffect(() => {
    if (!loaded || (route.hash && !route.restore)) return;
    let top = 0;
    try {
      top = Number(sessionStorage.getItem(`scroll:${view}`)) || 0;
    } catch {}
    const restore = () => main.current && (main.current.scrollTop = top);
    restore();
    // The views that load data get their full height later.
    const timer = setTimeout(restore, 250);
    return () => clearTimeout(timer);
  }, [view, loaded, route.seq]);
  const onScroll = (event) => {
    if (event.target !== main.current) return;
    try {
      sessionStorage.setItem(`scroll:${view}`, String(main.current.scrollTop));
    } catch {}
  };

  // The user moves the line between the main view and the session panel.
  const resize = (event) => {
    event.preventDefault();
    const from = event.clientX;
    const panel = event.currentTarget.nextElementSibling;
    const start = panel.offsetWidth;
    // The style sheet limits the width. Keep the width that the page shows.
    const move = (e) => {
      panel.style.width = `${Math.max(320, Math.round(start + from - e.clientX))}px`;
      setPanelWidth(panel.offsetWidth);
    };
    const stop = () => {
      document.body.classList.remove('resizing');
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', stop);
      window.removeEventListener('pointercancel', stop);
    };
    // The class stops the selection of text while the user moves the line.
    document.body.classList.add('resizing');
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', stop);
    window.addEventListener('pointercancel', stop);
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
            ? `Triagent ${run.review ? 'review ' : ''}run${run.model === MODELS[0] ? '' : ` with ${run.model}`}: ${run.activity || 'started'}`
            : status && status.next_run
              ? `Next triagent run ${until(status.next_run)}`
              : run
                ? `Last triagent run ${ago(run.ended)} ago${run.success ? '' : ' (failed)'}`
                : 'No triagent run yet'}
        </span>
        <button class="plain" onClick=${() => api.post(`/v1/triage/review?model=${reviewModel}`).then(() => say(`Asked for a review run with ${reviewModel}`), (e) => say(e.message, true))}
          title="Ask triagent to read all the sessions of the last 24 hours, and to complete the open items">
          Review
        </button>
        <${ModelSelect} value=${reviewModel} set=${setReviewModel} title="The model of the review run" />
        <button class="plain" onClick=${() => setChatOpen(!chatOpen)} title="Show or hide the chat (c)">${chatOpen ? 'Hide chat' : 'Chat'}</button>
      </div>
      <div class="body">
        <div class="main" ref=${main} onClick=${ctx.onRef} onScroll=${onScroll}>
          <div class="inner">
            ${view === 'items' && html`<${ItemsView} ctx=${ctx} />`}
            ${view === 'checks' && html`<${ChecksView} ctx=${ctx} />`}
            ${view === 'activity' && html`<${ActivityView} ctx=${ctx} seen=${seen} setSeen=${setSeen} />`}
          </div>
        </div>
        ${route.session &&
        html`<div class="resizer" onPointerDown=${resize} title="Move to change the width of the session panel"></div>
          <${SessionPanel} key=${route.session} ctx=${ctx} chain=${route.session} turn=${route.turn} refName=${route.ref}
            written=${written} width=${panelWidth} close=${() => go({ session: null, turn: null, ref: null })} />`}
        ${chatOpen && html`<${Chat} ctx=${ctx} handle=${chat} events=${chatEvents} />`}
      </div>
      ${hover && html`<${RefPopover} ctx=${ctx} hover=${hover} />`}
      ${toast && html`<div class=${`toast ${toast.isError ? 'error' : ''}`}>${toast.text}</div>`}
      ${restarted &&
      html`<div class="modal">
        <div class="box">
          <h2>The recap server started again</h2>
          <p>This page is from the server process before. The server now refuses the changes and the chat messages of this page.</p>
          <p>Load the page again. The page keeps the expanded items, the scroll position and the drafts.</p>
          <div class="actions">
            <span class="spacer"></span>
            <button class="plain" onClick=${() => setRestarted(false)}>Not now</button>
            <button class="primary" onClick=${() => location.reload()}>Load the page again</button>
          </div>
        </div>
      </div>`}
    </div>
  `;
}

render(html`<${App} />`, document.getElementById('app'));
