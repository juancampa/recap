// A small Markdown renderer for the prompts and the responses of the
// sessions, and for the chat. It returns HTML. It escapes all the text, so
// the result is safe to insert in the page.

import { escapeHtml as escape, findRefs, refHtml } from '/web/text.js';

// The lookup of a text without triage data.
const NO_DATA = { items: new Map(), topics: new Map(), repos: new Map(), chainOf: () => null };

// `env` is `{look, repo}`: the lookup of the triage data and the GitHub
// page of the repository that the text is about (see text.js).
function inline(text, env) {
  // Take the code spans, the links and the references out first, so the
  // other rules do not change them.
  const kept = [];
  const keep = (htmlText) => `\u0000${kept.push(htmlText) - 1}\u0000`;
  let s = text.replace(/`([^`\n]+)`/g, (_, code) => keep(`<code>${escape(code)}</code>`));
  s = s.replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, (_, label, url) =>
    keep(`<a href="${escape(url)}" target="_blank" rel="noreferrer">${escape(label)}</a>`),
  );
  const look = (env && env.look) || NO_DATA;
  const refs = findRefs(s, look, env && env.repo);
  for (let i = refs.length - 1; i >= 0; i--) {
    s = s.slice(0, refs[i].start) + keep(refHtml(refs[i], look)) + s.slice(refs[i].end);
  }
  s = escape(s);
  s = s.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/(^|[\s(])\*([^*\n]+)\*(?=[\s).,;:!?]|$)/g, '$1<em>$2</em>');
  return s.replace(/\u0000(\d+)\u0000/g, (_, i) => kept[Number(i)]);
}

function table(rows, env) {
  const cells = (line) => line.replace(/^\s*\|/, '').replace(/\|\s*$/, '').split('|').map((c) => c.trim());
  const head = cells(rows[0]);
  const body = rows.slice(2).map(cells);
  const th = head.map((c) => `<th>${inline(c, env)}</th>`).join('');
  const tr = body.map((r) => `<tr>${r.map((c) => `<td>${inline(c, env)}</td>`).join('')}</tr>`).join('');
  return `<table><thead><tr>${th}</tr></thead><tbody>${tr}</tbody></table>`;
}

export function markdown(text, env) {
  const lines = (text || '').replace(/\r/g, '').split('\n');
  const out = [];
  let i = 0;
  let paragraph = [];
  const flush = () => {
    if (paragraph.length) {
      out.push(`<p>${paragraph.map((l) => inline(l, env)).join('<br>')}</p>`);
      paragraph = [];
    }
  };
  while (i < lines.length) {
    const line = lines[i];
    const fence = line.match(/^\s*(```|~~~)/);
    if (fence) {
      flush();
      const code = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence[1])) code.push(lines[i++]);
      i++;
      out.push(`<pre><code>${escape(code.join('\n'))}</code></pre>`);
      continue;
    }
    if (!line.trim()) {
      flush();
      i++;
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flush();
      const level = Math.min(heading[1].length + 2, 6);
      out.push(`<h${level}>${inline(heading[2], env)}</h${level}>`);
      i++;
      continue;
    }
    if (/^\s*\|.*\|\s*$/.test(line) && /^\s*\|[\s:|-]+\|\s*$/.test(lines[i + 1] || '')) {
      flush();
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) rows.push(lines[i++]);
      out.push(table(rows, env));
      continue;
    }
    if (/^\s*>/.test(line)) {
      flush();
      const quote = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quote.push(lines[i++].replace(/^\s*>\s?/, ''));
      out.push(`<blockquote>${markdown(quote.join('\n'), env)}</blockquote>`);
      continue;
    }
    const item = line.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      flush();
      // One list. A line with more indent continues the item before it.
      const ordered = /\d/.test(item[2]);
      const items = [];
      while (i < lines.length) {
        const m = lines[i].match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
        if (m) {
          items.push({ indent: m[1].length, text: [m[3]] });
        } else if (lines[i].trim() && /^\s+/.test(lines[i]) && items.length) {
          items[items.length - 1].text.push(lines[i].trim());
        } else {
          break;
        }
        i++;
      }
      const base = Math.min(...items.map((x) => x.indent));
      const tag = ordered ? 'ol' : 'ul';
      const li = items
        .map((x) => `<li class="${x.indent > base ? 'nested' : ''}">${x.text.map((l) => inline(l, env)).join('<br>')}</li>`)
        .join('');
      out.push(`<${tag}>${li}</${tag}>`);
      continue;
    }
    if (/^\s*(-{3,}|\*{3,})\s*$/.test(line)) {
      flush();
      out.push('<hr>');
      i++;
      continue;
    }
    paragraph.push(line);
    i++;
  }
  flush();
  return out.join('\n');
}
