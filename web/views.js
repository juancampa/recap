// The views of the triage web UI: Items, Checks, Activity, and the
// session panel.

import { html, useState, useEffect, useRef, useMemo } from '/web/vendor/preact.js';
import { api, closed, short, ago, until, clock, useStored, useSession } from '/web/app.js';
import { markdown } from '/web/markdown.js';
import { Linked, TopicRef, hrefOf, quoteRange } from '/web/text.js';
import { highlight } from '/web/highlight.js';

const MARKS = { open: '○', claimed: '◐', unknown: '?', verified: '✓', confirmed: '✓', dropped: '–' };
const STATE_TEXT = {
  open: 'Open',
  claimed: 'An agent said that the item is done. No check confirmed it.',
  unknown: 'The check gave no result.',
  verified: 'The check confirmed that the item is done.',
  confirmed: 'You confirmed that the item is done.',
  dropped: 'Dropped',
};
const REF_KIND = { origin: 'Origin', claim: 'Done claim', context: 'Context' };
const IMPACTS = ['critical', 'high', 'normal', 'low'];
const IMPACT_LABEL = { critical: 'crit', high: 'high', normal: 'norm', low: 'low' };
const OUTCOME_CLASS = { done: 'done', 'not-done': '', unknown: 'bad', unapproved: 'bad', missing: 'bad' };

const sessionTitle = (session) => (session && (session.title || short(session.chain))) || '';
// `repo` is the GitHub page of the repository that the text is about.
const Md = ({ ctx, text, repo }) => {
  const __html = useMemo(() => markdown(text, { look: ctx.look, repo }), [text, ctx.look, repo]);
  return html`<div class="md" dangerouslySetInnerHTML=${{ __html }}></div>`;
};
const repoOf = (ctx, chain) => {
  const session = chain ? ctx.index.sessions.get(chain) : null;
  return session ? session.repo : undefined;
};

// ── The turns of a session ────────────────────────────────────────────────

// The key holds the last turn, so a new turn of the session gives a new
// request.
const turnCache = new Map();

function loadTurns(chain, lastTurn) {
  const key = `${chain}:${lastTurn || 0}`;
  if (!turnCache.has(key)) {
    const request = api
      .get(`/v1/history?agent=${encodeURIComponent(chain)}&kind=prompts`)
      .then((response) =>
        response.groups
          .flatMap((group) => group.entries)
          .filter((entry) => entry.kind.type === 'prompt')
          .sort((a, b) => (a.ordinal || 0) - (b.ordinal || 0) || a.timestamp.localeCompare(b.timestamp)),
      );
    request.catch(() => turnCache.delete(key));
    turnCache.set(key, request);
  }
  return turnCache.get(key);
}

function useTurns(ctx, chain) {
  const session = ctx.index.sessions.get(chain);
  const lastTurn = session && session.last_turn;
  const [turns, setTurns] = useState(null);
  const [error, setError] = useState(null);
  useEffect(() => {
    let live = true;
    loadTurns(chain, lastTurn).then(
      (t) => live && setTurns(t),
      (e) => live && setError(e.message),
    );
    return () => (live = false);
  }, [chain, lastTurn]);
  return [turns, error];
}

// A long text shows its start only, with a button that shows all of it.
// `open` shows all of the text from the start.
function Clip({ ctx, text, cls, repo, open }) {
  const long = text.length > 1400 || text.split('\n').length > 22;
  const [all, setAll] = useState(Boolean(open));
  return html`
    <div class=${`${cls} ${long && !all ? 'clip' : ''}`}><${Md} ctx=${ctx} text=${text} repo=${repo} /></div>
    ${long && html`<button class="plain" onClick=${() => setAll(!all)}>${all ? 'Show less' : 'Show all'}</button>`}
  `;
}

// The address of the session panel for the reference `index` of `item`.
const refName = (item, index) => `${item.id}/${index}`;
const refHref = (item, index) => {
  const r = item.refs[index];
  return `?session=${encodeURIComponent(r.chain)}&turn=${r.turn}&ref=${encodeURIComponent(refName(item, index))}`;
};
const openRef = (ctx, item, index) => (event) => {
  if (event.metaKey || event.ctrlKey) return;
  event.preventDefault();
  event.stopPropagation();
  ctx.openSession(item.refs[index].chain, item.refs[index].turn, refName(item, index));
};

// One turn of a session. `quote` is a text of the turn that the page
// marks. With `scroll`, the page also scrolls to the mark, or to the turn
// when the turn does not hold the quote.
function Turn({ ctx, chain, entry, hit, quote, scroll }) {
  const repo = repoOf(ctx, chain);
  const env = { ctx, repo, open: Boolean(quote) };
  const el = useRef(null);
  useEffect(() => {
    if (!el.current) return;
    const range = quote && window.Highlight && CSS.highlights ? quoteRange(el.current, quote) : null;
    if (scroll) {
      if (range) range.startContainer.parentElement.scrollIntoView({ block: 'center' });
      else el.current.scrollIntoView({ block: 'start' });
    }
    if (!range) return;
    // The style sheet gives the color of the highlight `quote`.
    let marks = CSS.highlights.get('quote');
    if (!marks) CSS.highlights.set('quote', (marks = new Highlight()));
    marks.add(range);
    return () => marks.delete(range);
  }, [quote, scroll, entry.kind.text, entry.kind.response]);
  return html`
    <div class=${`turn ${hit ? 'hit' : ''}`} ref=${el}>
      <div class="n"><span>Turn ${entry.ordinal}</span><span>${clock(entry.timestamp)}</span></div>
      <${Clip} ...${env} cls="q a" text=${entry.kind.text || ''} />
      ${entry.kind.response
        ? html`<${Clip} ...${env} cls="a" text=${entry.kind.response} />`
        : html`<div class="note" style="padding:4px 0">No final response for this turn.</div>`}
    </div>
  `;
}

// The reference `index` of `item`, in the details of the item: the kind,
// the turn and the quote. A click opens the turn in the session panel.
function TurnRef({ ctx, item, index }) {
  const turnRef = item.refs[index];
  const session = ctx.index.sessions.get(turnRef.chain);
  return html`
    <div class="turn-ref">
      <span class="kind">${REF_KIND[turnRef.kind || 'context']}</span>
      turn ${turnRef.turn}${turnRef.chain !== item.chain ? ` of ${sessionTitle(session) || short(turnRef.chain)}` : ''}
      ${' '}
      <a class="quote" href=${refHref(item, index)} onClick=${openRef(ctx, item, index)} title="Show the sentence in the session">
        ${turnRef.quote ? `“${turnRef.quote}”` : 'Open in the session'}
      </a>
    </div>
  `;
}

// ── One item ──────────────────────────────────────────────────────────────

function CheckRun({ label, spec, run }) {
  if (!spec) return null;
  const line = [spec.script, ...(spec.args || [])].join(' ');
  return html`
    <div class="k">${label}</div>
    <div class="v">
      <code>${line}</code>${' '}
      ${run
        ? html`<span class=${`chip ${OUTCOME_CLASS[run.outcome]}`}>${run.outcome}</span> <span class="age" style="width:auto">${ago(run.at)} ago${run.exit === undefined || run.exit === null ? '' : `, exit ${run.exit}`}</span>`
        : html`<span class="chip">no run yet</span>`}
      ${run && run.output && html`<pre>${run.output}</pre>`}
    </div>
  `;
}

// The state of an item as a sentence. For the state `claimed`, the first
// sentence is a link to the newest reference that holds the statement.
function StateText({ ctx, item }) {
  const claim = item.state === 'claimed' && item.refs ? item.refs.findLastIndex((r) => r.kind === 'claim') : -1;
  if (claim < 0) return STATE_TEXT[item.state];
  const [said, rest] = STATE_TEXT.claimed.split(/(?<=\.) /);
  return html`<a href=${refHref(item, claim)} onClick=${openRef(ctx, item, claim)} title="Show the statement in the session">${said}</a> ${rest}`;
}

function ItemDetail({ ctx, item }) {
  const session = item.chain ? ctx.index.sessions.get(item.chain) : null;
  const [dropping, setDropping] = useState(false);
  const [reason, setReason] = useState('');
  const [deleting, setDeleting] = useState(false);
  const isClosed = closed(item);
  const repo = session ? session.repo : undefined;
  // A text with its references as links. The key replaces the element
  // when the text changes.
  const linked = (text) => html`<${Linked} key=${text} ctx=${ctx} text=${text} repo=${repo} />`;
  const needsWhy = (item.impact === 'critical' || item.impact === 'high') && !item.why;
  const copyAndGo = () => ctx.copy(item.prompt).then(() => ctx.goToSession(item.chain));
  const drop = () => ctx.setItem(item.id, { status: 'dropped', note: reason || item.note }).then((ok) => ok && setDropping(false));

  return html`
    <div class="detail">
      <div class="fields">
        <div class="k">State</div>
        <div class="v"><${StateText} ctx=${ctx} item=${item} />${item.late ? ' The item is late.' : ''}</div>
        ${(item.why || needsWhy) &&
        html`<div class="k">Why</div>
          <div class=${`v ${item.why ? '' : 'missing'}`}>${item.why ? linked(item.why) : 'No reason recorded.'}</div>`}
        ${item.done_when && html`<div class="k">Done when</div><div class="v">${linked(item.done_when)}</div>`}
        <${CheckRun} label="Check" spec=${item.check} run=${item.check_run} />
        ${item.before &&
        html`<div class="k">Before</div>
          <div class="v">${linked(item.before.event)}${item.event_run && item.event_run.outcome === 'done' ? html` <span class="chip late">occurred</span>` : ''}</div>`}
        ${item.before && html`<${CheckRun} label="Event" spec=${item.before.check} run=${item.event_run} />`}
        ${item.due && html`<div class="k">Due</div><div class="v">${clock(item.due)} (${until(item.due)})</div>`}
        ${item.note && html`<div class="k">Note</div><div class="v"><${Md} ctx=${ctx} text=${item.note} repo=${repo} /></div>`}
        ${session &&
        html`<div class="k">Session</div>
          <div class="v">
            <button class="chip" onClick=${() => ctx.openSession(item.chain)}>
              <span class=${`dot ${session.status || ''}`}></span>${sessionTitle(session)}
            </button>
            ${' '}${session.summary && linked(session.summary)}
          </div>`}
        ${item.thread &&
        html`<div class="k">Chat</div>
          <div class="v">
            Triagent asked for this item in a chat.${' '}
            <button onClick=${() => ctx.openThread(item.thread)}>Open the conversation</button>
          </div>`}
        ${item.prompt &&
        html`<div class="k">Prompt</div>
          <div class="v">
            <div class="prompt">${linked(item.prompt)}</div>
            <div class="actions" style="margin-top:4px">
              <button onClick=${() => ctx.copy(item.prompt)}>Copy</button>
              ${item.chain && html`<button onClick=${copyAndGo}>Copy and go to the session</button>`}
              ${item.chain &&
              html`<button onClick=${() => ctx.writePrompt(item.chain, item.prompt)} title="Write the prompt in the session panel. You can change it there and send it to the session.">
                Edit and send
              </button>`}
            </div>
          </div>`}
        ${item.refs &&
        item.refs.length > 0 &&
        html`<div class="k">References</div>
          <div class="v">${item.refs.map((r, index) => html`<${TurnRef} key=${`${r.chain}:${r.turn}:${r.quote || ''}`} ctx=${ctx} item=${item} index=${index} />`)}</div>`}
      </div>
      <div class="actions">
        ${!isClosed && html`<button class="primary" onClick=${() => ctx.setItem(item.id, { status: 'confirmed' })}>Confirm done</button>`}
        ${(item.status !== 'open') && html`<button onClick=${() => ctx.setItem(item.id, { status: 'open' })}>Reopen</button>`}
        ${item.status !== 'dropped' && !dropping && html`<button onClick=${() => setDropping(true)}>Drop</button>`}
        ${dropping &&
        html`<input type="text" placeholder="Reason" value=${reason} style="width:240px"
            onInput=${(e) => setReason(e.target.value)}
            onKeyDown=${(e) => e.key === 'Enter' && drop()} />
          <button onClick=${drop}>Drop the item</button>
          <button class="plain" onClick=${() => setDropping(false)}>Cancel</button>`}
        <select value=${item.impact} onChange=${(e) => ctx.setItem(item.id, { impact: e.target.value })} title="The impact of the item">
          ${IMPACTS.map((i) => html`<option value=${i}>${i}</option>`)}
        </select>
        <select value=${item.owner} onChange=${(e) => ctx.setItem(item.id, { owner: e.target.value })} title="Who must do the item">
          <option value="user">you</option>
          <option value="agent">agent</option>
        </select>
        <button onClick=${() => ctx.ask(`@item:${item.id}`)}>Ask triagent</button>
        ${session &&
        html`<button onClick=${() => ctx.setSession(item.chain, { muted_turn: session.muted ? null : session.last_turn })}
            title="A muted session shows only its critical items. The mute ends at the next turn of the session.">
            ${session.muted ? 'Unmute the session' : 'Mute the session'}
          </button>`}
        <span class="spacer"></span>
        ${deleting
          ? html`<button onClick=${() => ctx.apply([{ op: 'delete_item', id: item.id }])}>Delete permanently</button>
              <button class="plain" onClick=${() => setDeleting(false)}>Cancel</button>`
          : html`<button class="plain" onClick=${() => setDeleting(true)}>Delete</button>`}
      </div>
    </div>
  `;
}

function ItemRow({ ctx, item, selected, expanded, onToggle, showTopic, showSession, elRef }) {
  const session = item.chain ? ctx.index.sessions.get(item.chain) : null;
  const topic = item.in_topic ? ctx.index.topics.get(item.in_topic) : null;
  const run = item.check_run;
  return html`
    <div class=${`item ${selected ? 'sel' : ''} ${closed(item) ? 'closed' : ''}`} ref=${elRef}>
      <div class="row" onClick=${(e) => !e.target.closest('a') && onToggle()}>
        <span class=${`mark ${item.state}`} title=${STATE_TEXT[item.state]}>${MARKS[item.state]}</span>
        <span class=${`impact ${item.impact}`}>${IMPACT_LABEL[item.impact]}</span>
        <span class=${`owner ${item.owner}`}>${item.owner === 'user' ? 'you' : 'agent'}</span>
        <span class="what">
          <span class="text" title=${item.text}>
            <${Linked} key=${item.text} ctx=${ctx} text=${item.text} repo=${session ? session.repo : undefined} /><span class="id">${item.id}</span>
          </span>
          ${item.late && html`<span class="chip late">late</span>`}
          ${!item.late && item.due && !closed(item) && html`<span class="chip">due ${until(item.due)}</span>`}
          ${!item.late && item.before && !closed(item) && html`<span class="chip" title=${`Before: ${item.before.event}`}>before ${item.before.event}</span>`}
          ${run && run.outcome !== 'not-done' && html`<span class=${`chip ${OUTCOME_CLASS[run.outcome]}`} title="The outcome of the check">${run.outcome}</span>`}
        </span>
        ${showTopic &&
        html`<span class="topic">
          ${topic &&
          html`<a class="chip tc" style=${topic.style} href=${`/items?group=topic#${item.id}`} title="Show the item with the other items of its topic"
            onClick=${(e) => !e.metaKey && !e.ctrlKey && (e.preventDefault(), ctx.go({ group: 'topic', hash: item.id }))}>
            ${topic.title}
          </a>`}
        </span>`}
        <span class="meta">
          ${session &&
          showSession &&
          html`<button class="chip" title=${session.summary || 'Open the session'}
              onClick=${(e) => (item.refs && item.refs.length ? openRef(ctx, item, item.refs.length - 1)(e) : (e.stopPropagation(), ctx.openSession(item.chain)))}>
              <span class=${`dot ${session.status || ''}`}></span>${sessionTitle(session)}
            </button>`}
          <span class="age">${ago(item.created)}</span>
        </span>
      </div>
      ${expanded && html`<${ItemDetail} ctx=${ctx} item=${item} />`}
    </div>
  `;
}

// A list of items with a selection that the keys move.
// `group` is the grouping of the view: a list in a group does not show
// the value that all its items have in common.
function ItemList({ ctx, items, sel, expanded, toggle, group, rowRefs }) {
  const showTopic = !group;
  return html`
    <div class=${`list ${showTopic ? 'cols' : ''}`}>
      ${items.map(
        (item) => html`<${ItemRow} key=${item.id} ctx=${ctx} item=${item} selected=${sel === item.id}
          expanded=${expanded.has(item.id)} onToggle=${() => toggle(item.id)} showTopic=${showTopic} showSession=${group !== 'session'}
          elRef=${(el) => (el ? rowRefs.current.set(item.id, el) : rowRefs.current.delete(item.id))} />`,
      )}
    </div>
  `;
}

// The selection and the expanded items of the view `name`. The browser
// keeps the expanded items for this tab, so a reload shows them again.
function useSelection(name) {
  const [sel, setSel] = useState(null);
  const [list, setList] = useSession(`expanded:${name}`, []);
  const expanded = useMemo(() => new Set(list), [list]);
  const rowRefs = useRef(new Map());
  const toggle = (id) => {
    setSel(id);
    setList(expanded.has(id) ? list.filter((x) => x !== id) : [...list, id]);
  };
  const expand = (id) => !expanded.has(id) && setList([...list, id]);
  const collapseAll = () => setList([]);
  const scrollTo = (id) => {
    const el = rowRefs.current.get(id);
    if (el) el.scrollIntoView({ block: 'nearest' });
  };
  return { sel, setSel, expanded, toggle, expand, collapseAll, rowRefs, scrollTo };
}

const CollapseAll = ({ selection }) =>
  html`<button class="icon" onClick=${selection.collapseAll} disabled=${selection.expanded.size === 0}
    title="Hide the details of all the items" aria-label="Hide the details of all the items">
    <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
      <path d="M4 2l4 4 4-4M4 14l4-4 4 4" />
    </svg>
  </button>`;

// The keys of the Items view. The list shows when the pointer or the focus
// is on the button.
const Keys = () =>
  html`<span class="keys">
    <button class="icon" aria-label="Keyboard shortcuts">
      <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round">
        <rect x="1.5" y="3.5" width="13" height="9" rx="1.5" />
        <path d="M4 6.5h.01M6.7 6.5h.01M9.3 6.5h.01M12 6.5h.01M4 9.5h.01M12 9.5h.01M6.5 9.5h3" />
      </svg>
    </button>
    <div class="pop">
      <div><kbd>j</kbd> <kbd>k</kbd> Move the selection</div>
      <div><kbd>Enter</kbd> Show or hide the details</div>
      <div><kbd>a</kbd> Ask triagent about the item</div>
      <div><kbd>/</kbd> Go to the filter</div>
      <div><kbd>c</kbd> Go to the chat</div>
      <div><kbd>1</kbd> <kbd>2</kbd> <kbd>3</kbd> Show a view</div>
      <div><kbd>Esc</kbd> Close the session panel</div>
    </div>
  </span>`;

// The groupings of the Items view. The address holds the grouping.
const GROUPS = [
  ['', 'None'],
  ['topic', 'Topic'],
  ['session', 'Session'],
];

// ── The Items view ──────────────────────────────────────────────────────────

export function ItemsView({ ctx }) {
  const { state, index, route, go } = ctx;
  const [owner, setOwner] = useStored('items-owner', 'user');
  const group = route.group;
  const groupRefs = useRef(new Map());
  const [showMuted, setShowMuted] = useStored('items-muted', false);
  // The address holds the text filter.
  const query = route.q;
  const setQuery = (q) => go({ q }, true);
  const [closedOpen, setClosedOpen] = useSession('items-closed-open', false);
  const [closedLimit, setClosedLimit] = useState(50);
  const selection = useSelection('items');
  const { sel, setSel, expanded, expand, rowRefs, scrollTo } = selection;
  const filter = useRef(null);
  // The address holds the item that the user opened last.
  const toggle = (id) => {
    go({ hash: expanded.has(id) ? '' : id }, true);
    selection.toggle(id);
  };

  const matches = (item) => {
    if (owner !== 'all' && item.owner !== owner) return false;
    const session = item.chain ? index.sessions.get(item.chain) : null;
    if (query) {
      const topic = item.in_topic ? index.topics.get(item.in_topic) : null;
      const text = `${item.text} ${item.id} ${item.why} ${sessionTitle(session)} ${topic ? topic.title : ''}`.toLowerCase();
      if (!query.toLowerCase().split(/\s+/).every((word) => text.includes(word))) return false;
    }
    return true;
  };
  const isMuted = (item) => {
    const session = item.chain ? index.sessions.get(item.chain) : null;
    return session && session.muted && item.impact !== 'critical';
  };
  const all = state.items.filter(matches);
  const openAll = all.filter((i) => !closed(i));
  const mutedCount = openAll.filter(isMuted).length;
  const open = showMuted ? openAll : openAll.filter((i) => !isMuted(i));
  const done = all.filter(closed);
  const doneShown = closedOpen ? done.slice(0, closedLimit) : [];

  // The key of the group of an item: the topic or the session.
  const keyOf = (item, by) => (by === 'topic' ? item.in_topic : item.chain) || '';
  // The groups are in the order of the first item of each group.
  let groups = null;
  if (group) {
    const byKey = new Map();
    for (const item of open) {
      const key = keyOf(item, group);
      if (!byKey.has(key)) byKey.set(key, []);
      byKey.get(key).push(item);
    }
    groups = [...byKey.entries()];
  }
  // The address can name a topic, as `#topic:<id>`.
  const hashTopic = route.hash.startsWith('topic:') ? route.hash.slice(6) : null;

  const order = [...(groups ? groups.flatMap(([, items]) => items) : open), ...doneShown].map((i) => i.id);

  // Show the item of the address after each navigation.
  useEffect(() => {
    const id = route.hash;
    if (!id) return;
    if (hashTopic !== null) {
      if (!index.topics.has(hashTopic)) return ctx.say(`No topic ${hashTopic}`, true);
      if (group !== 'topic') go({ group: 'topic' }, true);
      // Remove the filters when they hide all the items of the topic.
      if (!open.some((i) => i.in_topic === hashTopic)) {
        if (!state.items.some((i) => !closed(i) && i.in_topic === hashTopic)) return ctx.say('The topic has no open item');
        setOwner('all');
        setShowMuted(true);
        if (query) setQuery('');
      }
      if (route.restore) return;
      setTimeout(() => {
        const el = groupRefs.current.get(hashTopic);
        if (el) el.scrollIntoView({ block: 'start' });
      }, 60);
      return;
    }
    const item = index.items.get(id);
    if (!item) return ctx.say(`No item ${id}`, true);
    // Remove only the filters that hide the item.
    if (owner !== 'all' && item.owner !== owner) setOwner('all');
    if (isMuted(item)) setShowMuted(true);
    if (query && !matches(item)) setQuery('');
    if (closed(item)) {
      setClosedOpen(true);
      setClosedLimit(Math.max(50, state.items.filter(closed).findIndex((i) => i.id === id) + 1));
    }
    setSel(id);
    expand(id);
    if (route.restore) return;
    setTimeout(() => {
      const el = rowRefs.current.get(id);
      if (el) el.scrollIntoView({ block: 'center' });
    }, 60);
  }, [route.seq]);

  useEffect(() => {
    const onKey = (event) => {
      const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(event.target.tagName);
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      const at = order.indexOf(sel);
      const move = (to) => {
        const id = order[Math.max(0, Math.min(order.length - 1, to))];
        if (id === undefined) return;
        setSel(id);
        scrollTo(id);
      };
      if (event.key === 'j' || event.key === 'ArrowDown') move(at + 1);
      else if (event.key === 'k' || event.key === 'ArrowUp') move(at < 0 ? 0 : at - 1);
      else if ((event.key === 'Enter' || event.key === 'o') && sel) toggle(sel);
      else if (event.key === 'a' && sel) ctx.ask(`@item:${sel}`);
      else if (event.key === '/') filter.current && filter.current.focus();
      else return;
      event.preventDefault();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [order.join(' '), sel]);

  const waiting = state.sessions.filter((s) => s.status === 'waiting');
  const listProps = { ctx, sel, expanded, toggle, rowRefs };

  return html`
    <div class="toolbar">
      <div class="seg">
        ${[
          ['user', 'Mine'],
          ['agent', 'Agents'],
          ['all', 'All'],
        ].map(([id, label]) => html`<button class=${owner === id ? 'on' : ''} onClick=${() => setOwner(id)}>${label}</button>`)}
      </div>
      <input type="text" placeholder="Filter ( / )" value=${query} ref=${filter}
        onInput=${(e) => setQuery(e.target.value)}
        onKeyDown=${(e) => e.key === 'Escape' && (setQuery(''), e.target.blur())} />
      <span class="label">Group</span>
      <div class="seg">
        ${GROUPS.map(([id, label]) => html`<button class=${group === id ? 'on' : ''} onClick=${() => go({ group: id }, true)}>${label}</button>`)}
      </div>
      ${(mutedCount > 0 || showMuted) &&
      html`<label title="The items of the sessions that you muted">
        <input type="checkbox" checked=${showMuted} onChange=${(e) => setShowMuted(e.target.checked)} /> Muted (${mutedCount})
      </label>`}
      <span class="spacer"></span>
      <${Keys} />
      <${CollapseAll} selection=${selection} />
    </div>

    ${waiting.length > 0 &&
    html`<div class="waiting">
      ${waiting.map(
        (s) => html`<div class="w" key=${s.chain}>
          <span class="dot waiting"></span>
          <span class="t">${sessionTitle(s)}</span>
          <span class="f">waits for ${s.waiting_for || 'you'}</span>
          <span class="spacer"></span>
          <button onClick=${() => ctx.openSession(s.chain)}>Read</button>
          <button class="primary" onClick=${() => ctx.goToSession(s.chain)}>Go to the session</button>
        </div>`,
      )}
    </div>`}

    ${open.length === 0 && html`<div class="note">No open item passes the filter.</div>`}
    ${groups
      ? groups.map(([key, items]) => {
          const topic = group === 'topic' ? index.topics.get(key) : null;
          const session = group === 'session' ? index.sessions.get(key) : null;
          const sessionTopic = session && session.topic ? index.topics.get(session.topic) : null;
          return html`<div class=${`group ${group === 'topic' && key === hashTopic ? 'marked' : ''}`} key=${key}
            ref=${(el) => (el ? groupRefs.current.set(key, el) : groupRefs.current.delete(key))}>
            <h3>
              ${topic && html`<${TopicRef} topic=${topic} />`}
              ${session &&
              html`<button class="chip title" onClick=${() => ctx.openSession(key)} title="Show the session">
                <span class=${`dot ${session.status || ''}`}></span>${sessionTitle(session)}
              </button>`}
              ${!topic && !session && html`<span class="title">${group === 'topic' ? 'No topic' : key ? short(key) : 'No session'}</span>`}
              <span>${items.length}</span>
              ${sessionTopic && html`<${TopicRef} topic=${sessionTopic} />`}
              <span class="sum">${topic ? topic.summary : session ? `${session.status === 'waiting' ? `Waits for ${session.waiting_for || 'you'}. ` : ''}${session.summary || ''}` : ''}</span>
              ${topic && html`<button class="plain" onClick=${() => ctx.ask(`@topic:${topic.id}`)}>Ask triagent</button>`}
              ${session && html`<button class="plain" onClick=${() => ctx.ask(`@session:${short(key)}`)}>Ask triagent</button>`}
            </h3>
            <${ItemList} ...${listProps} items=${items} group=${group} />
          </div>`;
        })
      : open.length > 0 && html`<div class="group"><${ItemList} ...${listProps} items=${open} group="" /></div>`}

    ${done.length > 0 &&
    html`<div class="group">
      <h3>
        <button class="plain" onClick=${() => setClosedOpen(!closedOpen)}>${closedOpen ? '▾' : '▸'} Closed (${done.length})</button>
      </h3>
      ${closedOpen &&
      html`<${ItemList} ...${listProps} items=${doneShown} group="" />
        ${done.length > closedLimit && html`<div class="more"><button class="plain" onClick=${() => setClosedLimit(closedLimit + 200)}>Show more</button></div>`}`}
    </div>`}
  `;
}

// ── The session panel ─────────────────────────────────────────────────────

// The short end of an item id such as `7b6d7bd0-f1`: the part after the
// session. An id that is a name has no short end.
const idEnd = (id) => {
  const end = id.match(/^[0-9a-f]{8}-([a-z0-9]{1,4})$/);
  return end ? end[1] : '';
};

// `refName` names the reference of an item that the panel shows, as
// `<item>/<index>`. The panel marks the quote of the reference in `turn`.
export function SessionPanel({ ctx, chain, turn, refName, written, width, close }) {
  const session = ctx.index.sessions.get(chain);
  // The prompt that the user writes for the session.
  const [draft, setDraft] = useSession(`prompt:${chain}`, '');
  const [sending, setSending] = useState(false);
  const input = useRef(null);
  // `written` is a prompt of an item that the user wants to send.
  useEffect(() => {
    if (!written || written.chain !== chain) return;
    setDraft(written.text);
    if (input.current) input.current.focus();
  }, [written]);
  // The server sends a prompt only to a session that waits for a prompt.
  const status = session && session.status;
  const blocked = status === 'busy' || status === 'waiting';
  const send = () => {
    const text = draft.trim();
    if (!text || sending || blocked) return;
    setSending(true);
    api
      .ui(`/sessions/${chain}/send`, { text })
      .then(
        (result) => {
          setDraft('');
          ctx.say(result.ran ? 'Sent the prompt to the session' : 'Opened a window that resumes the session with the prompt');
        },
        (e) => ctx.say(e.message, true),
      )
      .finally(() => setSending(false));
  };
  const [turns, error] = useTurns(ctx, chain);
  const end = useRef(null);
  const cut = refName ? refName.lastIndexOf('/') : -1;
  const refItem = cut > 0 ? ctx.look.items.get(refName.slice(0, cut)) : null;
  const shown = refItem && refItem.refs ? refItem.refs[Number(refName.slice(cut + 1))] : null;
  const quote = shown && shown.chain === chain && shown.turn === turn ? shown.quote : '';
  const [allItems, setAllItems] = useState(false);
  const topic = session && session.topic ? ctx.index.topics.get(session.topic) : null;
  const items = ctx.state.items.filter((i) => i.chain === chain && !closed(i));

  // Show the newest turn. The referenced turn scrolls to its own place.
  useEffect(() => {
    if (turns && !turn && end.current) end.current.scrollIntoView({ block: 'end' });
  }, [turns, turn, chain]);

  return html`
    <div class="drawer" style=${`width:${width}px`} onClick=${ctx.onRef}>
      <header>
        <span class=${`dot ${(session && session.status) || ''}`}></span>
        <h2>${sessionTitle(session) || short(chain)}</h2>
        <span class="spacer"></span>
        <button class="primary" onClick=${() => ctx.goToSession(chain)}>Go to the session</button>
        <button onClick=${() => ctx.ask(`@session:${short(chain)}`)}>Ask triagent</button>
        ${session &&
        html`<button onClick=${() => ctx.setSession(chain, { muted_turn: session.muted ? null : session.last_turn })}>
          ${session.muted ? 'Unmute' : 'Mute'}
        </button>`}
        <button class="plain" onClick=${close} title="Close (Esc)">✕</button>
      </header>
      <div class="sub">
        ${topic && html`<${TopicRef} topic=${topic} prefix=${true} />`}
        ${session && session.status === 'waiting' && html`<span class="pill wait">waits for ${session.waiting_for || 'you'}</span>`}
        ${session && session.summary && html`<${Linked} key=${session.summary} ctx=${ctx} text=${session.summary} repo=${session.repo} />`}
        ${shown &&
        html`<span class="shown">
          ${REF_KIND[shown.kind || 'context']} of <a class="ref" data-kind="item" data-id=${refItem.id} href=${hrefOf('item', refItem.id)}>${refItem.text}</a>
          ${!quote && shown.quote ? ' (the reference is in another turn)' : ''}
        </span>`}
        ${(allItems ? items : items.slice(0, 6)).map(
          (i) => html`<a class="ref" data-kind="item" data-id=${i.id} href=${hrefOf('item', i.id)}>
            ${i.impact === 'critical' || i.impact === 'high' ? `${i.impact}: ` : ''}${i.text}${idEnd(i.id) && html` <span class="id-end">${idEnd(i.id)}</span>`}
          </a>`,
        )}
        ${items.length > 6 && html`<button class="chip" onClick=${() => setAllItems(!allItems)}>${allItems ? 'Show fewer items' : `+${items.length - 6} open items`}</button>`}
      </div>
      <div class="turns">
        ${error && html`<div class="note">${error}</div>`}
        ${!turns && !error && html`<div class="note">Loading…</div>`}
        ${turns && turns.length === 0 && html`<div class="note">The history has no prompt for this session.</div>`}
        ${turns &&
        turns.map(
          (entry) => {
            const hit = turn === entry.ordinal;
            // A new quote makes a new element, so a long text shows all of it.
            return html`<${Turn} key=${`${entry.id}:${hit ? quote : ''}`} ctx=${ctx} chain=${chain} entry=${entry} hit=${hit}
              quote=${hit ? quote : ''} scroll=${hit} />`;
          },
        )}
        <div ref=${end}></div>
      </div>
      <div class="composer">
        <textarea ref=${input} value=${draft} placeholder="Write a prompt for this session."
          onInput=${(e) => setDraft(e.target.value)}
          onKeyDown=${(e) => e.key === 'Enter' && !e.shiftKey && !e.isComposing && (e.preventDefault(), send())}></textarea>
        <div class="bar">
          <span>
            ${blocked
              ? `The session ${status === 'busy' ? 'is busy' : `waits for ${session.waiting_for || 'you'}`}. The server sends a prompt only to an idle session.`
              : html`<kbd>Enter</kbd> send · <kbd>Shift</kbd>+<kbd>Enter</kbd> new line`}
          </span>
          <span class="spacer"></span>
          <button class="primary" onClick=${send} disabled=${sending || blocked || !draft.trim()}
            title=${status ? 'Write the prompt in the terminal of the session and press Enter there' : 'The session does not run. Open a new terminal window that resumes it with the prompt.'}>
            ${status ? 'Send to the session' : 'Resume with this prompt'}
          </button>
        </div>
      </div>
    </div>
  `;
}

// ── The Checks view ───────────────────────────────────────────────────────

// The lines of `after` with the mark `+` for each line that `before` does
// not have, and the lines that only `before` has with the mark `-`.
function diffLines(before, after) {
  const a = before.split('\n');
  const b = after.split('\n');
  const lcs = Array.from({ length: a.length + 1 }, () => new Uint16Array(b.length + 1));
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  // Each result is `[mark, index]`: the index of the line in `after`, or
  // in `before` for the mark `-`.
  const out = [];
  let i = 0;
  let j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) out.push([' ', j++]), i++;
    else if (j < b.length && (i >= a.length || lcs[i][j + 1] >= lcs[i + 1][j])) out.push(['+', j++]);
    else out.push(['-', i++]);
  }
  return out;
}

// One line of a script with the colors of its syntax.
const CodeLine = ({ tokens }) => tokens.map(([cls, text]) => (cls ? html`<span class=${`hl-${cls}`}>${text}</span>` : text));

function Script({ ctx, script, reload, marked, elRef }) {
  const [open, setOpen] = useState(!script.approved || marked);
  const after = useMemo(() => highlight(script.text), [script.text]);
  const before = useMemo(() => (script.approved_text ? highlight(script.approved_text) : null), [script.approved_text]);
  const approve = () =>
    api.ui(`/checks/${encodeURIComponent(script.name)}/approve`, {}).then(
      () => (ctx.say(`Approved ${script.name}`), reload()),
      (e) => ctx.say(e.message, true),
    );
  // An approved script shows no difference.
  const lines = useMemo(
    () => (before && !script.approved ? diffLines(script.approved_text, script.text) : after.map((_, at) => ['', at])),
    [script.text, script.approved_text, script.approved],
  );
  return html`
    <div class=${`script ${marked ? 'marked' : ''}`} ref=${elRef}>
      <header onClick=${(e) => !e.target.closest('a, button') && setOpen(!open)} title=${open ? 'Hide the script' : 'Show the script'}>
        <button class="plain" onClick=${() => setOpen(!open)}>${open ? '▾' : '▸'}</button>
        <a class="n" href=${`/checks#${encodeURIComponent(script.name)}`}
          onClick=${(e) => (e.preventDefault(), ctx.go({ view: 'checks', hash: script.name }))}>${script.name}</a>
        ${script.approved
          ? html`<span class="pill ok">approved</span>`
          : html`<span class="pill wait">${script.approved_text ? 'changed, needs your approval' : 'new, needs your approval'}</span>`}
        ${script.items.slice(0, 6).map((id) => html`<a class="ref" data-kind="item" data-id=${id} href=${hrefOf('item', id)}>${id}</a>`)}
        ${script.items.length > 6 && html`<span class="age" style="width:auto">+${script.items.length - 6} items</span>`}
        ${script.items.length === 0 && html`<span class="age" style="width:auto">no item uses it</span>`}
        <span class="spacer"></span>
        ${!script.approved && html`<button class="primary" onClick=${approve}>Approve this version</button>`}
      </header>
      ${open &&
      html`<pre>${lines.map(
        ([mark, at]) => html`<span class=${`line ${mark === '+' ? 'add' : mark === '-' ? 'del' : ''}`}>
          ${mark && `${mark} `}<${CodeLine} tokens=${(mark === '-' ? before : after)[at] || []} />${'\n'}</span>`,
      )}</pre>`}
    </div>
  `;
}

export function ChecksView({ ctx }) {
  const { route } = ctx;
  const rows = useRef(new Map());
  const [scripts, setScripts] = useState(null);
  const [error, setError] = useState(null);
  const unapproved = ((ctx.status && ctx.status.unapproved) || []).join(' ');
  const load = () => api.ui('/checks').then(setScripts, (e) => setError(e.message));
  useEffect(() => {
    load();
  }, [unapproved, ctx.state.version]);
  // Show the script of the address after each navigation.
  useEffect(() => {
    const el = scripts && route.hash && rows.current.get(route.hash);
    if (el) el.scrollIntoView({ block: 'start' });
  }, [route.seq, !scripts]);
  if (error) return html`<div class="note">${error}</div>`;
  if (!scripts) return html`<div class="note">Loading…</div>`;
  const waiting = scripts.filter((s) => !s.approved);
  return html`
    <div class="note" style="padding:0 0 10px">
      The server runs only the version of a check script that you approved. Read each line before you approve a script:
      the server runs it with your permissions.
    </div>
    ${waiting.length === 0 && html`<div class="note" style="padding:0 0 10px">No script waits for your approval.</div>`}
    ${[...waiting, ...scripts.filter((s) => s.approved)].map(
      (s) => html`<${Script} key=${`${s.name}:${s.approved}:${s.name === route.hash}`} ctx=${ctx} script=${s} reload=${load}
        marked=${s.name === route.hash}
        elRef=${(el) => (el ? rows.current.set(s.name, el) : rows.current.delete(s.name))} />`,
    )}
  `;
}

// ── The Activity view ─────────────────────────────────────────────────────

const brief = (value) => {
  if (value === undefined || value === null) return '∅';
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  return text.length > 90 ? `${text.slice(0, 90)}…` : text;
};

export function ChangeText({ change }) {
  if (!change.field) return html`<span>${change.new !== undefined ? 'new' : 'deleted'}</span>`;
  // The check runner writes the outcome of a check or of an event.
  const field = { check_run: 'check outcome', event_run: 'event outcome' }[change.field] || change.field;
  return html`<span><span class="f">${field}:</span> ${change.old !== undefined ? `${brief(change.old)} → ` : ''}${brief(change.new)}</span>`;
}

export function showChange(ctx, change) {
  if (change.kind === 'item') ctx.showItem(change.id);
  else if (change.kind === 'topic') ctx.showTopic(change.id);
  else ctx.openSession(change.id);
}

export function changeLabel(ctx, change) {
  if (change.kind === 'session') return `session ${sessionTitle(ctx.index.sessions.get(change.id)) || short(change.id)}`;
  if (change.kind === 'topic') return `topic ${change.id}`;
  return change.id;
}

export function ActivityView({ ctx, seen, setSeen }) {
  const [changes, setChanges] = useState(null);
  const [actor, setActor] = useState('all');
  // The changes after this number are new for the user.
  const seenAtOpen = useRef(seen);
  const last = ctx.state.last_change;
  useEffect(() => {
    api.ui(`/changes?after=${Math.max(0, last - 500)}`).then(
      (list) => {
        setChanges(list.reverse());
        setSeen(last);
      },
      (e) => ctx.say(e.message, true),
    );
  }, [last]);
  if (!changes) return html`<div class="note">Loading…</div>`;
  const shown = changes.filter((c) => actor === 'all' || c.actor === actor);
  return html`
    <div class="toolbar">
      <div class="seg">
        ${[
          ['all', 'All'],
          ['triagent', 'Triagent'],
          ['user', 'You'],
          ['runner', 'Check runner'],
        ].map(([a, label]) => html`<button class=${actor === a ? 'on' : ''} onClick=${() => setActor(a)}>${label}</button>`)}
      </div>
      <span class="age" style="width:auto">The last ${changes.length} changes of the triage data, newest first.</span>
    </div>
    ${shown.length === 0 && html`<div class="note">No change.</div>`}
    ${shown.length > 0 &&
    html`<div class="list">
      ${shown.map(
        (c) => html`<div class=${`change ${seenAtOpen.current !== null && c.seq > seenAtOpen.current ? 'new' : ''}`} key=${c.seq}>
          <span class="when">${clock(c.at)}</span>
          <span class="who">${c.actor === 'user' ? 'you' : c.actor === 'runner' ? 'check runner' : c.actor}</span>
          <span class="oid" title=${c.id} onClick=${() => showChange(ctx, c)}>${changeLabel(ctx, c)}</span>
          <span class="what" title=${c.field ? `${brief(c.old)} → ${brief(c.new)}` : ''}><${ChangeText} change=${c} /></span>
        </div>`,
      )}
    </div>`}
  `;
}

// ── The details of the reference under the pointer ────────────────────────

// `hover` is `{kind, id, rect}`: an item or a session, and the place of the
// reference in the window.
export function RefPopover({ ctx, hover }) {
  const { index, state } = ctx;
  const session = hover.kind === 'session' ? state.sessions.find((s) => s.chain.startsWith(hover.id)) : null;
  const item = hover.kind === 'item' ? index.items.get(hover.id) : null;
  if (!item && !session) return null;
  const of = item ? (item.chain ? index.sessions.get(item.chain) : null) : session;
  const topicId = item ? item.in_topic : session.topic;
  const topic = topicId ? index.topics.get(topicId) : null;
  // The place of an open item in the priority order.
  const place = item && !closed(item) ? state.items.indexOf(item) + 1 : 0;
  // Below the reference, or above it near the end of the window.
  const { rect } = hover;
  const below = rect.bottom < window.innerHeight - 150;
  const style = `left:${Math.max(8, Math.min(rect.left, window.innerWidth - 468))}px;${
    below ? `top:${rect.bottom + 5}px` : `bottom:${window.innerHeight - rect.top + 5}px`
  }`;
  return html`
    <div class=${`popover ${item && closed(item) ? 'closed' : ''}`} style=${style}>
      ${item
        ? html`<div class="head">
              <span class=${`mark ${item.state}`}>${MARKS[item.state]}</span>
              <span class=${`impact ${item.impact}`}>${IMPACT_LABEL[item.impact]}</span>
              <span class=${`owner ${item.owner}`}>${item.owner === 'user' ? 'you' : 'agent'}</span>
              ${place > 0 && html`<span class="age" style="width:auto" title="The place in the priority order">#${place}</span>`}
              ${item.late && html`<span class="chip late">late</span>`}
              <span class="spacer"></span>
              <span class="age" style="width:auto">${ago(item.created)}</span>
            </div>
            <div class="title">${item.text}</div>`
        : html`<div class="head">
              <span class=${`dot ${session.status || ''}`}></span>
              <span class="title">${sessionTitle(session)}</span>
              <span class="spacer"></span>
              <span class="age" style="width:auto">${ago(session.last_ts)}</span>
            </div>
            ${session.summary && html`<div>${session.summary}</div>`}`}
      <div class="foot">
        ${topic && html`<span class="chip tc" style=${topic.style}>${topic.title}</span>`}
        ${item && of && html`<span class="chip"><span class=${`dot ${of.status || ''}`}></span>${sessionTitle(of)}</span>`}
        ${item && html`<span class="id">${item.id}</span>`}
      </div>
    </div>
  `;
}
