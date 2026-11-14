// A basic syntax highlighter for the check scripts: shell and Python.

const SHELL = 'if then else elif fi for in do done while until case esac function return exit local export readonly set unset shift break continue trap source';
const PYTHON = 'def class import from as return if elif else for while in not and or is try except finally with lambda None True False raise pass break continue yield global assert del';
const VARIABLE = /\$\{[^}\n]*\}|\$[A-Za-z_][A-Za-z0-9_]*|\$[0-9@#?*!$-]/g;

// The lines of `text`. Each line is a list of `[class, text]` tokens. The
// class is `c` (comment), `s` (string), `v` (variable), `k` (keyword),
// `n` (number) or an empty string.
export function highlight(text) {
  const python = /^#!.*python/.test(text);
  const keywords = new Set((python ? PYTHON : SHELL).split(' '));
  const tokens = [];
  const push = (cls, value) => value && tokens.push([cls, value]);
  // A string of the shell shows its variables.
  const string = (value, variables) => {
    if (!variables) return push('s', value);
    let at = 0;
    VARIABLE.lastIndex = 0;
    for (let m; (m = VARIABLE.exec(value)); ) {
      push('s', value.slice(at, m.index));
      push('v', m[0]);
      at = m.index + m[0].length;
    }
    push('s', value.slice(at));
  };
  let i = 0;
  let plain = '';
  const flush = () => {
    push('', plain);
    plain = '';
  };
  while (i < text.length) {
    const ch = text[i];
    const before = i === 0 ? '\n' : text[i - 1];
    if (ch === '#' && /\s/.test(before)) {
      const end = text.indexOf('\n', i);
      flush();
      push('c', text.slice(i, end < 0 ? text.length : end));
      i = end < 0 ? text.length : end;
    } else if (ch === '"' || ch === "'") {
      // A single quote of the shell has no escape character.
      const escapes = ch === '"' || python;
      let end = i + 1;
      while (end < text.length && text[end] !== ch) end += escapes && text[end] === '\\' ? 2 : 1;
      flush();
      string(text.slice(i, end + 1), ch === '"' && !python);
      i = end + 1;
    } else if (ch === '\\' && !python) {
      plain += text.slice(i, i + 2);
      i += 2;
    } else if (ch === '$' && !python) {
      VARIABLE.lastIndex = i;
      const m = VARIABLE.exec(text);
      if (m && m.index === i) {
        flush();
        push('v', m[0]);
        i += m[0].length;
      } else {
        plain += ch;
        i++;
      }
    } else if (/[A-Za-z_0-9]/.test(ch) && !/[A-Za-z_0-9.-]/.test(before)) {
      let end = i;
      while (end < text.length && /[A-Za-z_0-9]/.test(text[end])) end++;
      const word = text.slice(i, end);
      const after = text[end] || '';
      if (keywords.has(word) && !/[-.=/]/.test(after)) flush(), push('k', word);
      else if (/^\d+$/.test(word) && !/[-./:]/.test(after)) flush(), push('n', word);
      else plain += word;
      i = end;
    } else {
      plain += ch;
      i++;
    }
  }
  flush();
  // Divide the tokens into lines.
  const lines = [[]];
  for (const [cls, value] of tokens) {
    value.split('\n').forEach((part, at) => {
      if (at > 0) lines.push([]);
      if (part) lines[lines.length - 1].push([cls, part]);
    });
  }
  return lines;
}
