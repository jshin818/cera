// The note editor: CodeMirror 6 over the raw Markdown (the file stays the source of truth), with an
// Obsidian-style live preview drawn in Cēra's style. Syntax hides on lines the cursor is not on.
import { EditorState, StateField, StateEffect, Compartment } from '@codemirror/state';
import { EditorView, Decoration, WidgetType, keymap, placeholder, drawSelection, dropCursor } from '@codemirror/view';
import { markdown, markdownLanguage } from '@codemirror/lang-markdown';
import { syntaxTree, indentUnit } from '@codemirror/language';
import { history, defaultKeymap, historyKeymap, indentMore, indentLess } from '@codemirror/commands';
import { autocompletion, completionKeymap, startCompletion } from '@codemirror/autocomplete';
import { search, searchKeymap, highlightSelectionMatches } from '@codemirror/search';
import { tagColor } from './util.js';

/* ---------- Markdown extensions: [[wikilinks]], ![[embeds]], ==highlight==, #tags ---------- */
const WikiExt = {
  defineNodes: [{ name: 'WikiLink' }, { name: 'Embed' }, { name: 'Highlight' }, { name: 'HashTag' }],
  parseInline: [
    {
      name: 'WikiLink',
      before: 'Link',
      parse(cx, next, pos) {
        const embed = next === 33 && cx.char(pos + 1) === 91 && cx.char(pos + 2) === 91;
        if (!(embed || (next === 91 && cx.char(pos + 1) === 91))) return -1;
        const start = embed ? pos + 3 : pos + 2;
        for (let i = start; i < cx.end; i++) {
          const c = cx.char(i);
          if (c === 10) return -1;
          if (c === 93 && cx.char(i + 1) === 93) {
            if (i === start) return -1;
            return cx.addElement(cx.elt(embed ? 'Embed' : 'WikiLink', pos, i + 2));
          }
        }
        return -1;
      },
    },
    {
      name: 'Highlight',
      parse(cx, next, pos) {
        if (next !== 61 || cx.char(pos + 1) !== 61 || cx.char(pos + 2) === 32) return -1;
        for (let i = pos + 2; i < cx.end - 1; i++) {
          if (cx.char(i) === 10) return -1;
          if (cx.char(i) === 61 && cx.char(i + 1) === 61 && i > pos + 2) return cx.addElement(cx.elt('Highlight', pos, i + 2));
        }
        return -1;
      },
    },
    {
      name: 'HashTag',
      parse(cx, next, pos) {
        if (next !== 35) return -1;
        const prev = pos > cx.offset ? cx.char(pos - 1) : 32;
        if (!(prev === 32 || prev === 9 || prev === 10 || prev === 40 || pos === cx.offset)) return -1;
        let i = pos + 1, letters = 0;
        for (; i < cx.end; i++) {
          const ch = String.fromCharCode(cx.char(i));
          if (!/[\p{L}\p{N}_\-/]/u.test(ch)) break;
          if (!/[0-9]/.test(ch)) letters++;
        }
        if (i === pos + 1 || letters === 0) return -1;
        return cx.addElement(cx.elt('HashTag', pos, i));
      },
    },
  ],
};

/* ---------- widgets ---------- */
class CheckWidget extends WidgetType {
  constructor(done, pos) { super(); this.done = done; this.pos = pos }
  eq(o) { return o.done === this.done && o.pos === this.pos }
  toDOM(view) {
    const b = document.createElement('span');
    b.className = 'cm-check' + (this.done ? ' on' : '');
    b.setAttribute('role', 'checkbox');
    b.setAttribute('aria-checked', String(this.done));
    b.onmousedown = e => {
      e.preventDefault();
      view.dispatch({ changes: { from: this.pos + 1, to: this.pos + 2, insert: this.done ? ' ' : 'x' } });
    };
    return b;
  }
  ignoreEvent() { return true }
}
class BulletWidget extends WidgetType {
  constructor(ch) { super(); this.ch = ch }
  eq(o) { return o.ch === this.ch }
  toDOM() { const s = document.createElement('span'); s.className = 'cm-bullet-dot'; s.textContent = this.ch; return s }
}
class LabelWidget extends WidgetType {
  constructor(text) { super(); this.text = text }
  eq(o) { return o.text === this.text }
  toDOM() { const s = document.createElement('span'); s.className = 'cm-callout-label'; s.textContent = this.text; return s }
}
class HrWidget extends WidgetType {
  eq() { return true }
  toDOM() { const s = document.createElement('span'); s.className = 'cm-hr'; return s }
}
class PropsWidget extends WidgetType {
  constructor(summary) { super(); this.summary = summary }
  eq(o) { return o.summary === this.summary }
  toDOM(view) {
    const s = document.createElement('div');
    const b = document.createElement('span');
    b.className = 'cm-props';
    b.innerHTML = '<b>Properties</b>';
    b.append(document.createTextNode(this.summary));
    b.title = 'Click to edit the frontmatter';
    b.onmousedown = e => { e.preventDefault(); view.dispatch({ selection: { anchor: 4 } }); view.focus() };
    s.append(b);
    return s;
  }
  ignoreEvent() { return true }
}
class EmbedWidget extends WidgetType {
  constructor(target, opts) { super(); this.target = target; this.opts = opts }
  eq(o) { return o.target === this.target }
  toDOM() {
    const wrap = document.createElement('span');
    wrap.className = 'cm-embed';
    const t = this.target.split('|')[0].split('#')[0].trim();
    const isImg = /\.(png|jpe?g|gif|webp|svg|avif|bmp)$/i.test(t);
    this.opts.resolve(t).then(path => {
      if (!path) { wrap.className = 'cm-embed note'; wrap.innerHTML = `<b>${esc(t)}</b>Not created yet`; return }
      if (isImg) {
        const img = document.createElement('img');
        img.src = '/file/' + path.split('/').map(encodeURIComponent).join('/');
        img.alt = t; img.loading = 'lazy';
        const w = /\|(\d+)/.exec(this.target);
        if (w) img.style.width = w[1] + 'px';
        wrap.append(img);
      } else if (/\.md$/i.test(path) || !/\.\w+$/.test(path)) {
        wrap.className = 'cm-embed note';
        this.opts.excerpt(path, this.target.split('#')[1]).then(text => { wrap.innerHTML = `<b>${esc(path.replace(/\.md$/, '').split('/').pop())}</b>${esc(text)}` });
      } else {
        wrap.className = 'cm-embed note';
        wrap.innerHTML = `<b>${esc(t)}</b><a href="/file/${path.split('/').map(encodeURIComponent).join('/')}" target="_blank">Open file</a>`;
      }
    });
    return wrap;
  }
  ignoreEvent() { return false }
}
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

/* ---------- decorations ---------- */
const hide = Decoration.replace({});
const resolvedEffect = StateEffect.define();

function activeLines(state) {
  const set = new Set();
  if (!state.field(focusField, false)) return set;
  for (const r of state.selection.ranges) {
    const a = state.doc.lineAt(r.from).number, b = state.doc.lineAt(r.to).number;
    for (let i = a; i <= b; i++) set.add(i);
  }
  return set;
}

const focusEffect = StateEffect.define();
const focusField = StateField.define({
  create: () => false,
  update(v, tr) { for (const e of tr.effects) if (e.is(focusEffect)) return e.value; return v },
});

function frontmatterRange(doc) {
  if (doc.lines < 2 || doc.line(1).text !== '---') return null;
  for (let i = 2; i <= Math.min(doc.lines, 400); i++) {
    const t = doc.line(i).text;
    if (t === '---' || t === '...') return { from: 0, to: doc.line(i).to, endLine: i };
  }
  return null;
}

function buildDecorations(state, opts) {
  const doc = state.doc;
  const active = activeLines(state);
  const ranges = [];
  const add = (from, to, deco) => { if (from <= to) ranges.push(deco.range(from, to)) };
  const lineDeco = (pos, cls) => ranges.push(Decoration.line({ class: cls }).range(doc.lineAt(pos).from));
  const lineActive = pos => active.has(doc.lineAt(pos).number);

  const fm = frontmatterRange(doc);
  if (fm) {
    const fmActive = [...active].some(n => n <= fm.endLine);
    if (fmActive) {
      for (let i = 1; i <= fm.endLine; i++) lineDeco(doc.line(i).from, 'cm-frontmatter');
    } else {
      const body = doc.sliceString(0, fm.to);
      const keys = [...body.matchAll(/^([A-Za-z_][\w-]*):/gm)].map(m => m[1]);
      add(fm.from, fm.to, Decoration.replace({ widget: new PropsWidget(keys.length ? ' · ' + keys.join(', ') : ''), block: true }));
    }
  }

  // A leading "# Title" that repeats the note's title (file name) is hidden: the title field above
  // already shows it. It reappears when the cursor is on that line.
  const bodyStart = fm ? Math.min(doc.length, fm.to + 1) : 0;
  const titleLine = doc.lineAt(bodyStart);
  const t = opts.title?.();
  if (t && titleLine.text.replace(/^#\s+/, '').trim() === t && /^#\s/.test(titleLine.text) && !active.has(titleLine.number)) {
    add(titleLine.from, Math.min(doc.length, titleLine.to + 1), Decoration.replace({ block: true }));
  }
  let skipUntil = -1;
  syntaxTree(state).iterate({
    enter(node) {
      const { from, to, name } = node;
      if (fm && from < fm.to) return name === 'Document';
      if (from < skipUntil) return false;
      if (t && from >= titleLine.from && to <= titleLine.to && !active.has(titleLine.number) && /^#\s/.test(titleLine.text) && titleLine.text.replace(/^#\s+/, '').trim() === t) return false;
      const act = lineActive(from);
      switch (name) {
        case 'ATXHeading1': case 'ATXHeading2': case 'ATXHeading3': case 'ATXHeading4': case 'ATXHeading5': case 'ATXHeading6': {
          const lvl = +name.slice(-1);
          lineDeco(from, `cm-hline-${Math.min(lvl, 3)}`);
          add(from, to, Decoration.mark({ class: `cm-h${lvl}` }));
          break;
        }
        case 'HeaderMark':
          if (!act) add(from, Math.min(to + 1, doc.lineAt(from).to), hide);
          else add(from, to, Decoration.mark({ class: 'cm-mark' }));
          break;
        case 'StrongEmphasis': add(from, to, Decoration.mark({ class: 'cm-strong' })); break;
        case 'Emphasis': add(from, to, Decoration.mark({ class: 'cm-em' })); break;
        case 'Strikethrough': add(from, to, Decoration.mark({ class: 'cm-strike' })); break;
        case 'EmphasisMark': case 'StrikethroughMark':
          if (!act) add(from, to, hide); else add(from, to, Decoration.mark({ class: 'cm-mark' }));
          break;
        case 'Highlight':
          add(from, to, Decoration.mark({ class: 'cm-hl' }));
          if (!act) { add(from, from + 2, hide); add(to - 2, to, hide) }
          break;
        case 'InlineCode':
          add(from, to, Decoration.mark({ class: 'cm-icode' }));
          return false;
        case 'CodeMark':
          if (node.node.parent?.name === 'InlineCode') { if (!act) add(from, to, hide) }
          else add(from, to, Decoration.mark({ class: 'cm-mark' }));
          break;
        case 'FencedCode': case 'CodeBlock': {
          const first = doc.lineAt(from).number, last = doc.lineAt(to).number;
          for (let i = first; i <= last; i++) lineDeco(doc.line(i).from, 'cm-codeline' + (i === first ? ' cm-codeline-first' : '') + (i === last ? ' cm-codeline-last' : ''));
          if (name === 'FencedCode') {
            const info = node.node.getChild('CodeInfo');
            if (info) add(info.from, info.to, Decoration.mark({ class: 'cm-mark' }));
          }
          return false;
        }
        case 'Blockquote': {
          const first = doc.lineAt(from).number, last = doc.lineAt(to).number;
          const firstLine = doc.line(first);
          const cm = /^>\s*\[!([\w-]+)\][+-]?\s*(.*)$/.exec(firstLine.text);
          for (let i = first; i <= last; i++) lineDeco(doc.line(i).from, cm ? 'cm-quote cm-callout' : 'cm-quote');
          if (cm && !active.has(first)) {
            // "> [!note] Title" renders as a small label; the marker reappears when editing the line.
            const at = firstLine.from + firstLine.text.indexOf('[!');
            const endMarker = firstLine.text.indexOf(']', at - firstLine.from) + firstLine.from + 1;
            const label = cm[2] || cm[1][0].toUpperCase() + cm[1].slice(1);
            add(at, firstLine.to, Decoration.replace({ widget: new LabelWidget(label) }));
            void endMarker;
            skipUntil = firstLine.to;
          }
          break;
        }
        case 'QuoteMark':
          if (!act) add(from, Math.min(to + 1, doc.lineAt(from).to), hide);
          break;
        case 'HorizontalRule':
          if (!act) add(from, to, Decoration.replace({ widget: new HrWidget() }));
          break;
        case 'ListMark': {
          const item = node.node.parent;
          const task = item?.getChild('Task');
          const ordered = item?.parent?.name === 'OrderedList';
          if (task) {
            const marker = task.getChild('TaskMarker');
            if (marker) {
              const done = /x/i.test(doc.sliceString(marker.from, marker.to));
              add(from, marker.to + (doc.sliceString(marker.to, marker.to + 1) === ' ' ? 1 : 0), Decoration.replace({ widget: new CheckWidget(done, marker.from) }));
              if (done) add(marker.to, task.to, Decoration.mark({ class: 'cm-done' }));
            }
          } else if (!ordered && !act) {
            add(from, to, Decoration.replace({ widget: new BulletWidget('•') }));
          } else {
            add(from, to, Decoration.mark({ class: 'cm-bullet' }));
          }
          break;
        }
        case 'TaskMarker': return false;
        case 'Link': {
          const url = node.node.getChild('URL');
          const href = url ? doc.sliceString(url.from, url.to) : '';
          const marks = node.node.getChildren('LinkMark');
          if (!act && marks.length >= 2) {
            add(from, marks[0].to, hide);
            add(marks[1].from, to, hide);
          }
          if (marks.length >= 2) add(marks[0].to, marks[1].from, Decoration.mark({ class: 'cm-url', attributes: { 'data-href': href } }));
          return false;
        }
        case 'Image': {
          if (act) return false;
          const url = node.node.getChild('URL');
          if (url) {
            const src = doc.sliceString(url.from, url.to);
            add(from, to, Decoration.replace({ widget: new EmbedWidget(/^https?:/.test(src) ? src : decodeURIComponent(src), opts) }));
          }
          return false;
        }
        case 'WikiLink': {
          const inner = doc.sliceString(from + 2, to - 2);
          const pipe = inner.indexOf('|');
          const target = (pipe >= 0 ? inner.slice(0, pipe) : inner).trim();
          const shown = pipe >= 0 ? inner.slice(pipe + 1) : inner;
          const state_ = opts.linkState(target);
          const cls = 'cm-wikilink' + (state_ === false ? ' unresolved' : '');
          if (!act) {
            add(from, from + 2 + (pipe >= 0 ? pipe + 1 : 0), hide);
            add(to - 2 - 0, to, hide);
            add(from + 2 + (pipe >= 0 ? pipe + 1 : 0), to - 2, Decoration.mark({ class: cls, attributes: { 'data-target': target } }));
          } else {
            add(from, to, Decoration.mark({ class: cls, attributes: { 'data-target': target } }));
          }
          void shown;
          return false;
        }
        case 'Embed': {
          const target = doc.sliceString(from + 3, to - 2);
          if (!act) add(from, to, Decoration.replace({ widget: new EmbedWidget(target, opts) }));
          else add(from, to, Decoration.mark({ class: 'cm-wikilink', attributes: { 'data-target': target.split('|')[0] } }));
          return false;
        }
        case 'HashTag': {
          const tag = doc.sliceString(from + 1, to).toLowerCase();
          add(from, to, Decoration.mark({ class: 'cm-tag', attributes: { 'data-c': String(tagColor(tag)), 'data-tag': tag } }));
          return false;
        }
        case 'Table':
          for (let i = doc.lineAt(from).number; i <= doc.lineAt(to).number; i++) lineDeco(doc.line(i).from, 'cm-table');
          return false;
      }
    },
  });
  // Block ids (^id at line end) are dimmed.
  const text = doc.toString();
  for (const m of text.matchAll(/ \^[A-Za-z0-9-]+$/gm)) add(m.index + 1, m.index + m[0].length, Decoration.mark({ class: 'cm-blockid' }));

  try {
    return Decoration.set(ranges, true);
  } catch {
    // Overlapping replacements from unusual syntax: drop replacements that overlap a previous one.
    ranges.sort((a, b) => a.from - b.from || a.value.startSide - b.value.startSide);
    const ok = [];
    let lastReplaceEnd = -1;
    for (const r of ranges) {
      const isReplace = r.value.spec && (r.value.spec.widget || r.from < r.to) && r.value.constructor.name !== 'MarkDecoration';
      if (isReplace && r.from < lastReplaceEnd) continue;
      if (isReplace) lastReplaceEnd = r.to;
      ok.push(r);
    }
    return Decoration.set(ok, true);
  }
}

function livePreview(opts) {
  return StateField.define({
    create: state => buildDecorations(state, opts),
    update(deco, tr) {
      if (tr.docChanged || tr.selection || tr.effects.some(e => e.is(resolvedEffect) || e.is(focusEffect)) || syntaxTree(tr.state) !== syntaxTree(tr.startState)) {
        return buildDecorations(tr.state, opts);
      }
      return deco.map(tr.changes);
    },
    provide: f => EditorView.decorations.from(f),
  });
}

/* ---------- commands ---------- */
function wrap(view, mark) {
  const changes = view.state.changeByRange(range => {
    const text = view.state.sliceDoc(range.from, range.to);
    const before = view.state.sliceDoc(range.from - mark.length, range.from);
    const after = view.state.sliceDoc(range.to, range.to + mark.length);
    if (before === mark && after === mark) {
      return { changes: [{ from: range.from - mark.length, to: range.from }, { from: range.to, to: range.to + mark.length }], range: { anchor: range.from - mark.length, head: range.to - mark.length } };
    }
    return { changes: { from: range.from, to: range.to, insert: mark + text + mark }, range: { anchor: range.from + mark.length, head: range.to + mark.length } };
  });
  view.dispatch(view.state.update(changes, { scrollIntoView: true }));
  return true;
}

function toggleTask(view) {
  const line = view.state.doc.lineAt(view.state.selection.main.head);
  const m = /^(\s*)([-*+]) \[( |x|X)\] /.exec(line.text);
  if (m) {
    const pos = line.from + m[1].length + 3;
    view.dispatch({ changes: { from: pos, to: pos + 1, insert: m[3] === ' ' ? 'x' : ' ' } });
  } else {
    const lm = /^(\s*)([-*+] )?/.exec(line.text);
    view.dispatch({ changes: { from: line.from + lm[1].length, to: line.from + lm[0].length, insert: '- [ ] ' } });
  }
  return true;
}

// Continue lists on Enter (bullets, numbers, tasks); an empty item ends the list.
function continueList(view) {
  const { state } = view;
  const sel = state.selection.main;
  if (!sel.empty) return false;
  const line = state.doc.lineAt(sel.head);
  const m = /^(\s*)([-*+]|\d+[.)])( \[[ xX]\])? (.*)$/.exec(line.text);
  if (!m || sel.head < line.from + m[0].length - m[4].length) return false;
  if (!m[4].trim()) {
    view.dispatch({ changes: { from: line.from, to: line.to, insert: '' } });
    return true;
  }
  let marker = m[2];
  const num = /^(\d+)([.)])$/.exec(marker);
  if (num) marker = `${+num[1] + 1}${num[2]}`;
  const ins = '\n' + m[1] + marker + (m[3] ? ' [ ]' : '') + ' ';
  view.dispatch({ changes: { from: sel.head, insert: ins }, selection: { anchor: sel.head + ins.length }, scrollIntoView: true });
  return true;
}

const SLASH = [
  { label: 'Heading 1', detail: '#', apply: '# ' },
  { label: 'Heading 2', detail: '##', apply: '## ' },
  { label: 'Heading 3', detail: '###', apply: '### ' },
  { label: 'To-do', detail: '- [ ]', apply: '- [ ] ' },
  { label: 'Bulleted list', detail: '-', apply: '- ' },
  { label: 'Numbered list', detail: '1.', apply: '1. ' },
  { label: 'Quote', detail: '>', apply: '> ' },
  { label: 'Callout', detail: '> [!note]', apply: '> [!note]\n> ' },
  { label: 'Code block', detail: '```', apply: '```\n\n```', cursor: 4 },
  { label: 'Divider', detail: '---', apply: '---\n' },
  { label: 'Table', detail: '| |', apply: '| Column | Column |\n| --- | --- |\n|  |  |', cursor: 37 },
  { label: 'Link to note', detail: '[[', apply: '[[', complete: true },
  { label: 'Highlight', detail: '==', apply: '====', cursor: 2 },
  { label: 'Today', detail: 'date', apply: () => new Date().toISOString().slice(0, 10) },
];

function completions(opts) {
  return async ctx => {
    // [[note links
    const link = ctx.matchBefore(/!?\[\[[^\]\n|#]*$/);
    if (link) {
      const q = link.text.replace(/^!?\[\[/, '');
      const hits = await opts.searchTitles(q);
      const from = link.from + link.text.indexOf('[[') + 2;
      const closeAfter = ctx.state.sliceDoc(ctx.pos, ctx.pos + 2) === ']]';
      return {
        from,
        filter: false,
        options: hits.map(h => ({
          label: h.title,
          detail: h.alias ? `alias of ${h.title}` : h.folder,
          apply: (view, c, f, t) => {
            const insert = (h.alias ? `${h.title}|${h.alias}` : h.title) + (closeAfter ? '' : ']]');
            view.dispatch({ changes: { from: f, to: t, insert }, selection: { anchor: f + insert.length + (closeAfter ? 2 : 0) } });
          },
        })).concat(q.trim() && !hits.some(h => h.title.toLowerCase() === q.trim().toLowerCase()) ? [{ label: q.trim(), detail: 'new note', apply: (view, c, f, t) => { const ins = q.trim() + (closeAfter ? '' : ']]'); view.dispatch({ changes: { from: f, to: t, insert: ins }, selection: { anchor: f + ins.length + (closeAfter ? 2 : 0) } }) } }] : []),
      };
    }
    // #tags
    const tag = ctx.matchBefore(/(?:^|\s)#[\p{L}\p{N}_\-/]*$/u);
    if (tag && tag.text.trim().length >= 2) {
      const from = tag.from + tag.text.indexOf('#') + 1;
      const q = ctx.state.sliceDoc(from, ctx.pos).toLowerCase();
      const tags = (await opts.tags()).filter(t => t.tag.startsWith(q) && t.tag !== q).slice(0, 12);
      if (!tags.length) return null;
      return { from, filter: false, options: tags.map(t => ({ label: t.tag, detail: String(t.count) })) };
    }
    // /commands at line start or after whitespace
    const slash = ctx.matchBefore(/(?:^|\s)\/[\w ]*$/);
    if (slash) {
      const from = slash.from + slash.text.indexOf('/');
      const q = ctx.state.sliceDoc(from + 1, ctx.pos).toLowerCase();
      const opts2 = SLASH.filter(s => s.label.toLowerCase().includes(q));
      if (!opts2.length) return null;
      return {
        from,
        filter: false,
        options: opts2.map(s => ({
          label: s.label,
          detail: s.detail,
          apply: (view, c, f, t) => {
            const text = typeof s.apply === 'function' ? s.apply() : s.apply;
            const line = view.state.doc.lineAt(f);
            const atStart = view.state.sliceDoc(line.from, f).trim() === '';
            const pre = atStart || /^[#\->1`|]/.test(text) === false ? '' : '\n';
            const insert = pre + text;
            view.dispatch({ changes: { from: f, to: t, insert }, selection: { anchor: f + pre.length + (s.cursor ?? text.length) } });
            if (s.complete) setTimeout(() => startCompletion(view), 0);
          },
        })),
      };
    }
    return null;
  };
}

/* ---------- public API ---------- */
export function createEditor(parent, opts) {
  const resolved = new Map(); // target(lowercase) -> path | null
  const pending = new Map();
  let view;
  const linkState = target => {
    const k = target.toLowerCase();
    if (!k) return true;
    if (resolved.has(k)) return resolved.get(k) !== null;
    if (!pending.has(k)) {
      pending.set(k, opts.resolve(target).then(p => {
        resolved.set(k, p);
        pending.delete(k);
        view?.dispatch({ effects: resolvedEffect.of(null) });
        return p;
      }));
    }
    return undefined;
  };
  const resolve = target => {
    const k = target.toLowerCase();
    if (resolved.has(k)) return Promise.resolve(resolved.get(k));
    linkState(target);
    return pending.get(k) || Promise.resolve(resolved.get(k));
  };
  const decoOpts = { linkState, resolve, excerpt: opts.excerpt, title: opts.title };
  const editable = new Compartment();

  const state = text => EditorState.create({
    doc: text,
    extensions: [
      history(),
      drawSelection(),
      dropCursor(),
      EditorView.lineWrapping,
      indentUnit.of('    '),
      markdown({ base: markdownLanguage, extensions: [WikiExt], addKeymap: true }),
      focusField,
      livePreview(decoOpts),
      highlightSelectionMatches(),
      search({ top: true }),
      autocompletion({ override: [completions(opts)], icons: false, closeOnBlur: true, activateOnTyping: true }),
      placeholder('Start writing… type / for blocks, [[ to link'),
      editable.of(EditorView.editable.of(true)),
      keymap.of([
        { key: 'Mod-b', run: v => wrap(v, '**') },
        { key: 'Mod-i', run: v => wrap(v, '*') },
        { key: 'Mod-Shift-x', run: v => wrap(v, '~~') },
        { key: 'Mod-Shift-h', run: v => wrap(v, '==') },
        { key: 'Mod-e', run: v => wrap(v, '`') },
        { key: 'Mod-Enter', run: toggleTask },
        { key: 'Enter', run: continueList },
        { key: 'Tab', run: indentMore, shift: indentLess },
        ...completionKeymap,
        ...searchKeymap,
        ...historyKeymap,
        ...defaultKeymap,
      ]),
      EditorView.updateListener.of(u => {
        if (u.docChanged) opts.onChange?.(u);
        if (u.focusChanged) u.view.dispatch({ effects: focusEffect.of(u.view.hasFocus) });
      }),
      EditorView.domEventHandlers({
        mousedown(e, v) {
          const link = e.target.closest('.cm-wikilink');
          const tagEl = e.target.closest('.cm-tag');
          const url = e.target.closest('.cm-url');
          const line = e.target.closest('.cm-line');
          const lineActive = line && v.hasFocus && v.state.selection.ranges.some(r => { const p = v.posAtDOM(line); return v.state.doc.lineAt(p).number === v.state.doc.lineAt(r.head).number });
          if (e.altKey) return false;
          if (link && (!lineActive || e.metaKey || e.ctrlKey)) { e.preventDefault(); opts.openLink(link.dataset.target, e.metaKey || e.ctrlKey); return true }
          if (tagEl && (!lineActive || e.metaKey || e.ctrlKey)) { e.preventDefault(); opts.openTag(tagEl.dataset.tag); return true }
          if (url && (!lineActive || e.metaKey || e.ctrlKey)) {
            e.preventDefault();
            const href = url.dataset.href;
            if (/^[a-z]+:/i.test(href)) window.open(href, '_blank', 'noopener'); else opts.openLink(decodeURIComponent(href), e.metaKey || e.ctrlKey);
            return true;
          }
          return false;
        },
        paste(e, v) {
          const files = [...(e.clipboardData?.files || [])];
          if (!files.length) return false;
          e.preventDefault();
          insertFiles(v, files);
          return true;
        },
        drop(e, v) {
          const files = [...(e.dataTransfer?.files || [])];
          if (!files.length) return false;
          e.preventDefault();
          const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
          if (pos != null) v.dispatch({ selection: { anchor: pos } });
          insertFiles(v, files);
          return true;
        },
      }),
    ],
  });

  async function insertFiles(v, files) {
    for (const f of files) {
      const path = await opts.upload(f);
      if (!path) continue;
      const name = path.split('/').pop();
      const ins = `![[${name}]]`;
      const at = v.state.selection.main.head;
      v.dispatch({ changes: { from: at, insert: ins }, selection: { anchor: at + ins.length } });
      resolved.set(name.toLowerCase(), path);
    }
  }

  view = new EditorView({ state: state(''), parent });

  return {
    view,
    get text() { return view.state.doc.toString() },
    set(text, { keepSelection = false } = {}) {
      const sel = view.state.selection.main;
      view.setState(state(text));
      if (keepSelection) {
        const a = Math.min(sel.anchor, text.length);
        view.dispatch({ selection: { anchor: a }, effects: focusEffect.of(view.hasFocus) });
      }
    },
    replaceAll(text) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
    },
    insertAtCursor(text) {
      const at = view.state.selection.main.head;
      view.dispatch({ changes: { from: at, insert: text }, selection: { anchor: at + text.length } });
    },
    focus(atEnd) {
      view.focus();
      if (atEnd) view.dispatch({ selection: { anchor: view.state.doc.length } });
    },
    reveal(pos) {
      view.dispatch({ selection: { anchor: Math.min(pos, view.state.doc.length) }, scrollIntoView: true });
      view.focus();
    },
    clearResolved() { resolved.clear(); view.dispatch({ effects: resolvedEffect.of(null) }) },
    setResolved(map) { for (const [k, v] of Object.entries(map)) resolved.set(k.toLowerCase(), v); view.dispatch({ effects: resolvedEffect.of(null) }) },
    hasFocus: () => view.hasFocus,
  };
}
