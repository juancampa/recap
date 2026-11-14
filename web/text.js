// The references in a text of the triage data: the items, the sessions, the
// topics, the pull requests on GitHub and the URLs. The Markdown renderer
// and the `Linked` component use the same rules.

import { html, useMemo } from '/web/vendor/preact.js';

export const escapeHtml = (s) =>
  String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

// Give each topic of `state` its color, as a CSS text in `style`. A color
// is one of 12 hues in one of 2 forms: a filled background, or an outline.
// The id of the topic gives the color. A topic whose color is in use gets
// a free color, so two topics have different colors while a free color
// exists.
const HUES = 12;
export function setTopicColors(state) {
  const used = new Set();
  for (const topic of [...state.topics].sort((x, y) => x.id.localeCompare(y.id))) {
    let hash = 2166136261;
    for (let i = 0; i < topic.id.length; i++) hash = Math.imul(hash ^ topic.id.charCodeAt(i), 16777619);
    let slot = (hash >>> 0) % (2 * HUES);
    // A step of 7 goes through all the 24 colors.
    for (let n = 0; n < 2 * HUES && used.has(slot); n++) slot = (slot + 7) % (2 * HUES);
    used.add(slot);
    const hue = (slot % HUES) * (360 / HUES);
    topic.style = slot < HUES ? `--h:${hue}` : `--h:${hue};--tc-fill:0;--tc-line:1px`;
  }
  return state;
}

// The address of the page that shows a triage object.
export function hrefOf(kind, id) {
  if (kind === 'item') return `/items#${encodeURIComponent(id)}`;
  if (kind === 'topic') return `/topics#${encodeURIComponent(id)}`;
  return `?session=${encodeURIComponent(id)}`;
}

// What the rules need from the triage data.
export function lookup(state) {
  const items = new Map(state.items.map((i) => [i.id, i]));
  const topics = new Map(state.topics.map((t) => [t.id, t]));
  const chains = new Map(state.sessions.map((s) => [s.chain.slice(0, 8), s.chain]));
  // The GitHub page of each repository, by the lower-case name.
  const repos = new Map();
  for (const s of state.sessions) if (s.repo) repos.set(s.repo.split('/').pop().toLowerCase(), s.repo);
  const chainOf = (word) => {
    const chain = chains.get(word.slice(0, 8));
    return chain && chain.startsWith(word) ? chain : null;
  };
  return { items, topics, repos, chainOf };
}

const EXPLICIT = /@(item|session|topic):([A-Za-z0-9._-]*[A-Za-z0-9])/g;
const URL_RE = /https?:\/\/[^\s<>)\]]*[^\s<>)\].,;:!?'"]/g;
// `owner/name#12`, `name PR 12`, `PR #12`, `pull request 12` and `#12`.
const FULL_PR = /\b([A-Za-z0-9][A-Za-z0-9-]*\/[A-Za-z0-9._-]+)#(\d{1,5})\b/g;
const NAMED_PR = /\b(?:([A-Za-z0-9._-]+)\s+)?(?:PRs?|pull requests?)\s*#?(\d{1,5})\b/g;
const NUMBER = /(^|[\s(,])#(\d{1,5})\b/g;
const WORD = /[A-Za-z0-9][A-Za-z0-9._-]*[A-Za-z0-9]/g;
const HEX = /^[0-9a-f]{8}(-[0-9a-f-]*)?$/;

// The references in `text`, in the order of the text. Each one is
// `{start, end, kind, id, label, href}`. The kind `link` has only an `href`.
// `repo` is the GitHub page of the repository that the text is about.
export function findRefs(text, look, repo) {
  const found = [];
  const add = (start, end, ref) => found.push({ start, end, label: text.slice(start, end), ...ref });
  const each = (re, f) => {
    re.lastIndex = 0;
    for (let m; (m = re.exec(text)); ) f(m);
  };
  // The rules are in the order of their priority: `take` drops a match
  // that has text in common with an earlier match.
  each(EXPLICIT, (m) => {
    const id = m[1] === 'session' ? look.chainOf(m[2]) || m[2] : m[2];
    add(m.index, m.index + m[0].length, { kind: m[1], id, explicit: true });
  });
  each(URL_RE, (m) => add(m.index, m.index + m[0].length, { kind: 'link', href: m[0] }));
  each(FULL_PR, (m) => add(m.index, m.index + m[0].length, { kind: 'link', href: `https://github.com/${m[1]}/pull/${m[2]}` }));
  each(NAMED_PR, (m) => {
    // The word before `PR` can name another repository.
    const named = m[1] && look.repos.get(m[1].toLowerCase());
    const page = named || repo;
    if (!page) return;
    const start = named ? m.index : m.index + m[0].indexOf(m[0].match(/PRs?|pull requests?/)[0]);
    add(start, m.index + m[0].length, { kind: 'link', href: `${page}/pull/${m[2]}` });
  });
  // GitHub sends `/issues/<n>` to the pull request when `<n>` is one.
  if (repo) each(NUMBER, (m) => add(m.index + m[1].length, m.index + m[0].length, { kind: 'link', href: `${repo}/issues/${m[2]}` }));
  each(WORD, (m) => {
    const word = m[0];
    if (word.includes('-') && look.items.has(word)) add(m.index, m.index + word.length, { kind: 'item', id: word });
    else if (HEX.test(word) && look.chainOf(word)) add(m.index, m.index + word.length, { kind: 'session', id: look.chainOf(word) });
  });
  const taken = [];
  for (const ref of found) {
    if (!taken.some((t) => ref.start < t.end && t.start < ref.end)) taken.push(ref);
  }
  return taken.sort((a, b) => a.start - b.start);
}

// The letters and the digits of `text` in lower case, and the index in
// `text` of each of them. A quote of a transcript and the text of the page
// are equal in this form: the Markdown signs and the white space are gone.
function plainChars(text) {
  let flat = '';
  const indexes = [];
  for (let i = 0; i < text.length; i++) {
    const ch = text[i].toLowerCase();
    if (ch.length === 1 && /[\p{L}\p{N}]/u.test(ch)) {
      flat += ch;
      indexes.push(i);
    }
  }
  return [flat, indexes];
}

// The range of the text `quote` in the element `root`, or null.
export function quoteRange(root, quote) {
  const [want] = plainChars(quote);
  if (!want) return null;
  let flat = '';
  const places = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node; (node = walker.nextNode()); ) {
    const [text, indexes] = plainChars(node.data);
    flat += text;
    for (const index of indexes) places.push([node, index]);
  }
  const at = flat.indexOf(want);
  if (at < 0) return null;
  const range = document.createRange();
  range.setStart(places[at][0], places[at][1]);
  const end = places[at + want.length - 1];
  range.setEnd(end[0], end[1] + 1);
  return range;
}

// The text that the page shows for a reference.
function labelOf(ref, look) {
  if (ref.kind === 'topic') {
    const topic = look.topics.get(ref.id);
    return `topic: ${topic ? topic.title : ref.id}`;
  }
  if (!ref.explicit) return ref.label;
  return ref.kind === 'session' ? `session ${ref.id.slice(0, 8)}` : ref.id;
}

// The HTML of a reference, for the Markdown renderer.
export function refHtml(ref, look) {
  if (ref.kind === 'link') {
    return `<a href="${escapeHtml(ref.href)}" target="_blank" rel="noreferrer">${escapeHtml(ref.label)}</a>`;
  }
  const known = ref.kind === 'topic' && look.topics.get(ref.id);
  const topic = known ? ` tc" style="${known.style}"` : '"';
  return `<a class="ref ref-${ref.kind}${topic} data-kind="${ref.kind}" data-id="${escapeHtml(ref.id)}" href="${escapeHtml(hrefOf(ref.kind, ref.id))}">${escapeHtml(labelOf(ref, look))}</a>`;
}

// A reference to a topic, in the color of the topic.
export function TopicRef({ topic, prefix }) {
  return html`<a class="ref ref-topic tc" style=${topic.style} data-kind="topic" data-id=${topic.id}
    href=${hrefOf('topic', topic.id)}>${prefix ? 'topic: ' : ''}${topic.title}</a>`;
}

// A plain text with its references as links. The page handles a click on
// `a.ref`. One element holds all the parts. Give the component the text as
// its `key`: then a new text replaces the element and all that it contains.
export function Linked({ ctx, text, repo }) {
  const look = ctx.look;
  const parts = useMemo(() => {
    const out = [];
    let at = 0;
    for (const ref of findRefs(text || '', look, repo)) {
      if (ref.start > at) out.push(text.slice(at, ref.start));
      out.push(ref);
      at = ref.end;
    }
    if (at < (text || '').length) out.push(text.slice(at));
    return out;
  }, [text, look, repo]);
  return html`<span class="linked">${parts.map((part) => {
    if (typeof part === 'string') return part;
    if (part.kind === 'link') return html`<a href=${part.href} target="_blank" rel="noreferrer">${part.label}</a>`;
    if (part.kind === 'topic') {
      const topic = look.topics.get(part.id);
      if (topic) return html`<${TopicRef} topic=${topic} prefix=${true} />`;
    }
    return html`<a class=${`ref ref-${part.kind}`} data-kind=${part.kind} data-id=${part.id} href=${hrefOf(part.kind, part.id)}>${labelOf(part, look)}</a>`;
  })}</span>`;
}
