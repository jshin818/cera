// Ask: a question in, an answer out, and every claim tied to the passage it came from. The server
// recalls the passages (vault-core) and streams the model's answer; this file renders the thread,
// the numbered sources beside it, how those sources link, and what the notes leave out.
import { api } from './api.js';
import { esc, SVG, ICON, ago, folderOf } from './util.js';

const MODELS = [['claude-opus-5-5', 'Opus 5.5'], ['claude-sonnet-5-5', 'Sonnet 5.5'], ['claude-haiku-5-5', 'Haiku 5.5']];
const modelName = id => MODELS.find(m => m[0] === id)?.[1] || id;

// The conversation outlives the view, so opening a source and coming back keeps it.
const thread = [];   // { q, recall, answer, model, note, error, state: 'recalling' | 'answering' | 'done' }
let shown = -1;      // turn whose sources are on the side
let ctrl = null;
let root = null;
let deps = null;
let aiState = null;

/* ---------- a small, safe Markdown renderer for answers ---------- */
function inline(s, srcs) {
  return esc(s)
    .replace(/`([^`\n]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*\n]+)\*\*/g, '<b>$1</b>')
    .replace(/(^|[^*\w])\*([^*\n]+)\*(?!\w)/g, '$1<i>$2</i>')
    .replace(/\[\[([^\]|#\n]+)(?:#[^\]|\n]*)?(?:\|([^\]\n]+))?\]\]/g, (_, t, label) => `<button class="wl" data-link="${t}">${label || t}</button>`)
    .replace(/\[(\d{1,2})\]/g, (m, n) => {
      const p = srcs?.find(x => x.n === +n);
      return p ? `<button class="cite" data-n="${n}" title="${esc(p.title)}${p.section ? ' › ' + esc(p.section) : ''}">${n}</button>` : m;
    });
}
export function renderMarkdown(src, srcs) {
  const out = [];
  let list = null, para = [], code = null;
  const flushPara = () => { if (para.length) out.push(`<p>${inline(para.join('\n'), srcs).replace(/\n/g, '<br>')}</p>`); para = [] };
  const flushList = () => { if (list) out.push(`</${list}>`); list = null };
  for (const line of src.split('\n')) {
    if (code !== null) {
      if (/^```/.test(line)) { out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`); code = null } else code.push(line);
      continue;
    }
    if (/^```/.test(line)) { flushPara(); flushList(); code = []; continue }
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    const li = /^\s*(?:[-*•]|(\d+)[.)])\s+(.*)$/.exec(line);
    if (h) { flushPara(); flushList(); out.push(`<h4>${inline(h[2], srcs)}</h4>`) }
    else if (li) {
      flushPara();
      const kind = li[1] ? 'ol' : 'ul';
      if (list !== kind) { flushList(); out.push(`<${kind}>`); list = kind }
      out.push(`<li>${inline(li[2], srcs)}</li>`);
    } else if (/^>\s?/.test(line)) { flushPara(); flushList(); out.push(`<blockquote>${inline(line.replace(/^>\s?/, ''), srcs)}</blockquote>`) }
    else if (!line.trim()) { flushPara(); flushList() }
    else { flushList(); para.push(line) }
  }
  if (code !== null) out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
  flushPara(); flushList();
  return out.join('');
}

/* ---------- the view ---------- */
export function mountAsk(ex, d, q0) {
  deps = d;
  root = ex;
  document.title = 'Ask — Cera';
  ex.innerHTML = `<div class="ask">
    <div class="ask-main">
      <div id="ask-thread" aria-live="polite"></div>
      <div class="ask-dock">
        <div class="composer"><textarea id="ask-q" rows="1" placeholder="Ask about your notes" aria-label="Ask about your notes"></textarea><button id="ask-send" class="send" aria-label="Ask" title="Ask (↵)" disabled><svg class="ic" viewBox="0 0 24 24"><path d="M12 19V5M6 11l6-6 6 6"/></svg></button></div>
        <div class="ask-foot"><button id="ask-model" class="ask-model" title="Answer model — change in Settings"></button><span class="grow"></span><button id="ask-new" class="ask-link" hidden>New question</button></div>
      </div>
    </div>
    <aside class="ask-side" id="ask-src" aria-label="Sources"></aside>
  </div>`;
  const q = ex.querySelector('#ask-q');
  const send = ex.querySelector('#ask-send');
  q.oninput = () => { send.disabled = !q.value.trim() };
  q.onkeydown = e => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); submit() } };
  send.onclick = submit;
  ex.querySelector('#ask-new').onclick = () => { ctrl?.abort(); thread.length = 0; shown = -1; draw(); q.focus() };
  ex.querySelector('#ask-model').onclick = () => deps.openSettings('ai');
  ex.onclick = onClick;
  loadAi();
  draw();
  if (q0) ask(q0); else q.focus();
}

async function loadAi() {
  aiState = await api.ai().catch(() => null);
  drawModel();
}

function drawModel(live) {
  const b = root?.querySelector('#ask-model');
  if (!b) return;
  const a = aiState;
  let label;
  if (live) label = live.provider === 'claude' ? `Claude ${modelName(live.model)}` : `Local · ${live.model}`;
  else if (!a || a.provider === 'off') label = 'Sources only';
  else if (a.provider === 'claude' || (a.provider === 'auto' && a.has_key)) label = a.has_key ? `Claude ${modelName(a.claude_model)}` : 'Claude · add a key';
  else if (a.provider === 'local') label = `Local · ${a.local_model || 'first model'}`;
  else label = 'Local model or sources only';
  const ready = live || (a && ((a.has_key && a.provider !== 'off' && a.provider !== 'local') || a.provider === 'local'));
  b.innerHTML = `<span class="dot${ready ? ' on' : ''}"></span>${esc(label)}`;
}

function submit() {
  const q = root.querySelector('#ask-q');
  const text = q.value.trim();
  if (!text) return;
  q.value = '';
  root.querySelector('#ask-send').disabled = true;
  ask(text);
}

async function ask(question) {
  ctrl?.abort();
  ctrl = new AbortController();
  const history = thread.filter(t => t.answer && !t.error).slice(-4).flatMap(t => [{ role: 'user', content: t.q }, { role: 'assistant', content: t.answer }]);
  const turn = { q: question, recall: null, answer: '', model: null, note: null, error: null, state: 'recalling', t0: performance.now() };
  thread.push(turn);
  shown = thread.length - 1;
  draw();
  let pending = false;
  const paint = () => { if (!pending) { pending = true; requestAnimationFrame(() => { pending = false; drawTurn(thread.indexOf(turn)) }) } };
  try {
    await api.ask({ question, history }, (ev, data) => {
      if (ev === 'sources') { turn.recall = data; turn.state = 'answering'; if (thread[shown] === turn) drawSources(); paint() }
      else if (ev === 'model') { turn.model = data; drawModel(data) }
      else if (ev === 'delta') { turn.answer += data.text; paint() }
      else if (ev === 'done') { turn.note = data.note || null; turn.state = 'done'; turn.ms = performance.now() - turn.t0; paint() }
      else if (ev === 'error') { turn.error = data.message; turn.state = 'done'; paint() }
    }, ctrl.signal);
  } catch (e) {
    if (e.name !== 'AbortError') { turn.error = e.message || String(e); }
  }
  if (turn.state !== 'done') { turn.state = 'done'; if (!turn.error && !turn.answer) turn.note = 'Stopped.' }
  paint();
}

/* ---------- drawing ---------- */
const alive = () => root && document.contains(root) && root.querySelector('#ask-thread');

function draw() {
  if (!alive()) return;
  const el = root.querySelector('#ask-thread');
  root.querySelector('#ask-new').hidden = !thread.length;
  if (!thread.length) {
    el.innerHTML = `<div class="ask-empty">
      <h1 class="ex-title">Ask your notes</h1>
      <p class="ex-sub">Answers come only from your Markdown notes, and every claim points to the passage it came from. Scope a question with <code>tag:project</code> or <code>path:Journal</code>.</p>
    </div>`;
  } else {
    el.innerHTML = thread.map((_, i) => `<section class="turn" data-t="${i}"></section>`).join('');
    thread.forEach((_, i) => drawTurn(i));
  }
  drawSources();
}

function drawTurn(i) {
  if (!alive() || i < 0) return;
  const t = thread[i];
  const el = root.querySelector(`.turn[data-t="${i}"]`);
  if (!el) return draw();
  const srcs = t.recall?.passages.map(p => ({ n: p.n, title: p.title, section: p.section })) || [];
  const n = t.recall?.passages.length ?? 0;
  let body;
  if (t.answer) body = `<div class="answer prose-a">${renderMarkdown(t.answer, srcs)}${t.state !== 'done' ? '<span class="caret"></span>' : ''}</div>`;
  else if (t.state === 'recalling') body = `<div class="answer muted"><span class="pulse"></span>Looking through your notes…</div>`;
  else if (t.state === 'answering') body = `<div class="answer muted"><span class="pulse"></span>Found ${n} ${n === 1 ? 'passage' : 'passages'}. Writing the answer…</div>`;
  else body = '';
  const sourcesOnly = t.state === 'done' && !t.answer && !t.error && n > 0;
  const meta = [];
  if (n) meta.push(`<button class="ask-link" data-show="${i}">${n} ${n === 1 ? 'source' : 'sources'}${shown === i ? '' : ' · show'}</button>`);
  if (t.model) meta.push(esc(t.model.provider === 'claude' ? `Claude ${modelName(t.model.model)}` : t.model.model));
  if (t.ms) meta.push(`${(t.ms / 1000).toFixed(1)} s`);
  el.classList.toggle('on', shown === i);
  el.innerHTML = `<div class="q">${esc(t.q)}</div>
    ${body}
    ${sourcesOnly ? `<div class="answer">${topPassages(t.recall)}</div>` : ''}
    ${t.note ? `<div class="ask-note">${esc(t.note)}${/Settings/.test(t.note) ? ` <button class="ask-link" data-settings>Set up answers</button>` : ''}</div>` : ''}
    ${t.error ? `<div class="ask-note err">${esc(t.error)}</div>` : ''}
    ${t.state === 'done' && (t.answer || n) ? `<div class="a-meta">${meta.join('<span class="sep">·</span>')}<span class="grow"></span>${t.answer ? `<button class="ask-link" data-copy="${i}">Copy</button>` : ''}<button class="ask-link" data-save="${i}">Save to Inbox</button></div>` : ''}`;
  if (i === thread.length - 1) {
    const sc = root.querySelector('#ask-thread');
    if (sc.scrollHeight - sc.scrollTop - sc.clientHeight < 160 || t.state !== 'done') sc.scrollTop = sc.scrollHeight;
  }
}

// With no model, the best passages are the answer.
function topPassages(r) {
  return `<p class="muted">The passages that best match, best first:</p>` + r.passages.slice(0, 3).map(p => `<button class="pass" data-n="${p.n}"><span class="src-n">${p.n}</span><span>${esc(clip(p.text, 220))}</span></button>`).join('');
}
const clip = (s, n) => (s.length > n ? s.slice(0, n).replace(/\s+\S*$/, '') + '…' : s);

function drawSources() {
  if (!alive()) return;
  const el = root.querySelector('#ask-src');
  const t = thread[shown];
  const r = t?.recall;
  if (!t) { el.innerHTML = `<div class="src-empty"><h3>Sources</h3><p>The passages an answer uses appear here, numbered, with where they live in your vault and why they were picked.</p></div>`; return }
  if (!r) { el.innerHTML = `<div class="src-empty"><h3>Sources</h3><p><span class="pulse"></span>Searching…</p></div>`; return }
  const paths = new Map(r.passages.map(p => [p.path, p.title]));
  el.innerHTML = `<h3>Sources <small>${r.passages.length} · ${r.took_ms.toFixed(0)} ms</small></h3>
    ${r.passages.map(p => `<article class="src" id="src-${p.n}" data-n="${p.n}">
      <header><span class="src-n">${p.n}</span><button class="src-title" data-open="${p.n}">${esc(p.title)}</button></header>
      <div class="src-where">${esc([folderOf(p.path), p.section].filter(Boolean).join(' › ') || p.path)} · line ${p.line} · ${esc(ago(p.mtime_ms))}</div>
      <button class="src-text" data-open="${p.n}" title="Open at this passage">${esc(p.text)}</button>
      ${p.why.length ? `<div class="src-why">${p.why.map(w => `<span>${esc(w)}</span>`).join('')}</div>` : ''}
    </article>`).join('')}
    ${r.connections.length ? `<h3>How they connect</h3><div class="src-links">${r.connections.map(c => `<div><button data-path="${esc(c.from)}">${esc(paths.get(c.from) || c.from)}</button><span>links to</span><button data-path="${esc(c.to)}">${esc(paths.get(c.to) || c.to)}</button></div>`).join('')}</div>` : ''}
    ${r.gaps.length ? `<h3>Gaps</h3><ul class="src-gaps">${r.gaps.map(g => `<li data-kind="${g.kind}">${esc(g.message)}${g.path ? ` <button class="ask-link" data-path="${esc(g.path)}">Open</button>` : ''}</li>`).join('')}</ul>` : ''}`;
}

function flash(n) {
  const card = root.querySelector(`#src-${n}`);
  if (!card) return;
  card.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  card.classList.remove('flash');
  void card.offsetWidth;
  card.classList.add('flash');
}

function onClick(e) {
  const b = e.target.closest('button');
  if (!b) return;
  const turn = b.closest('.turn');
  if (turn && +turn.dataset.t !== shown && (b.matches('.cite, .pass, [data-show]'))) { shown = +turn.dataset.t; draw() }
  const t = thread[shown];
  if (b.matches('.cite, .pass')) return flash(b.dataset.n);
  if (b.dataset.show) return;
  if (b.dataset.open) {
    const p = t?.recall?.passages.find(x => x.n === +b.dataset.open);
    if (p) deps.openSource(p);
    return;
  }
  if (b.dataset.path) return deps.openNote(b.dataset.path);
  if (b.dataset.link) return api.resolve(b.dataset.link, '').then(r => r?.path ? deps.openNote(r.path) : deps.toast(`No note named “${b.dataset.link}”.`)).catch(() => {});
  if (b.dataset.settings !== undefined) return deps.openSettings('ai');
  if (b.dataset.copy) return navigator.clipboard.writeText(thread[+b.dataset.copy].answer).then(() => deps.toast('Answer copied'), () => deps.toast('Copying is blocked here.'));
  if (b.dataset.save) return saveTurn(thread[+b.dataset.save]);
}

// A saved answer is an ordinary note whose sources are wikilinks, so it joins the graph.
async function saveTurn(t) {
  const r = t.recall;
  const link = p => {
    const target = p.path.replace(/\.md$/i, '');
    const heading = p.section?.split(' > ').pop();
    return `[[${target}${heading && heading !== p.title ? '#' + heading : ''}|${p.title}${heading && heading !== p.title ? ' › ' + heading : ''}]]`;
  };
  const sources = r?.passages.map(p => `${p.n}. ${link(p)} (line ${p.line})`).join('\n') || '';
  const body = `${t.answer || r?.passages.slice(0, 3).map(p => `> ${p.text.split('\n').join('\n> ')} [${p.n}]`).join('\n\n') || ''}\n\n## Sources\n\n${sources}\n`;
  try {
    const n = await api.create('Inbox', t.q.replace(/[\\/:*?"<>|#^[\]]+/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 90) || 'Answer', body);
    deps.refreshSidebar();
    deps.toast('Saved to Inbox', 'Open', () => deps.openNote(n.path));
  } catch (e) { deps.toast(e.message || String(e)) }
}

/* ---------- Settings → AI ---------- */
export async function renderAiSettings(el, toast, check) {
  const a = (aiState = await api.ai().catch(() => null));
  if (!a) { el.innerHTML = `<p class="s-note">Couldn't load AI settings.</p>`; return }
  const seg = (key, value, options) => `<span class="seg" role="radiogroup">${options.map(([v, label]) => `<button role="radio" aria-checked="${v === value}" class="${v === value ? 'on' : ''}" data-ai="${key}" data-v="${v}">${label}</button>`).join('')}</span>`;
  const row = (label, hint, control) => `<div class="s-row"><div><b>${label}</b>${hint ? `<small>${hint}</small>` : ''}</div>${control}</div>`;
  const keyHint = a.key_source === 'env' ? 'Using ANTHROPIC_API_KEY from the environment' : a.has_key ? 'Saved on this Mac, outside your vault' : 'Needed to answer with Claude';
  el.innerHTML = `
    ${row('Answer with', 'Auto uses Claude when a key is set, otherwise a local model if one is running', seg('provider', a.provider, [['auto', 'Auto'], ['claude', 'Claude'], ['local', 'Local'], ['off', 'Off']]))}
    ${row('Claude API key', keyHint, `<span class="s-field"><input id="ai-key" type="password" autocomplete="off" spellcheck="false" placeholder="${a.has_key ? '••••••••' : 'sk-ant-…'}" aria-label="Claude API key">${a.key_source === 'saved' ? `<button class="btn quiet" id="ai-key-del">Remove</button>` : ''}<button class="btn quiet" id="ai-key-save">Save</button></span>`)}
    ${row('Claude model', 'Opus is the most capable; Haiku is the fastest', seg('claude_model', a.claude_model, MODELS))}
    ${row('Local server', 'Ollama: localhost:11434 · LM Studio: localhost:1234', `<span class="s-field"><input id="ai-url" value="${esc(a.local_url)}" spellcheck="false" aria-label="Local model server URL"><button class="btn quiet" id="ai-check">Check</button></span>`)}
    <div id="ai-local"></div>
    <p class="s-note">Your question and the matching passages are sent to the model you pick: to Anthropic for Claude, or to the server on this Mac for a local model. Fallback to another Claude model is on, so an overloaded model doesn't fail the answer. Nothing is sent when answers are off.</p>`;
  const save = (patch, recheck) => api.setAi(patch).then(v => { aiState = v; drawModel(); return renderAiSettings(el, toast, recheck) }).catch(e => toast(e.message || String(e)));
  const checkLocal = async () => {
    const box = el.querySelector('#ai-local');
    box.innerHTML = `<p class="s-note">Checking…</p>`;
    const r = await api.aiModels(el.querySelector('#ai-url').value.trim()).catch(() => null);
    if (!r || !r.reachable) { box.innerHTML = `<p class="s-note">Nothing answered there. Start Ollama or LM Studio, then check again.</p>`; return }
    if (!r.models.length) { box.innerHTML = `<p class="s-note">The server answered but has no models. Pull one first (for example <code>ollama pull llama3.2</code>).</p>`; return }
    box.innerHTML = row('Local model', `${r.models.length} available`, `<select id="ai-model" aria-label="Local model">${['', ...r.models].map(m => `<option value="${esc(m)}"${m === a.local_model ? ' selected' : ''}>${m ? esc(m) : 'First listed'}</option>`).join('')}</select>`);
    box.querySelector('#ai-model').onchange = e => save({ local_model: e.target.value });
  };
  el.onclick = e => {
    const b = e.target.closest('[data-ai]');
    if (b) return save({ [b.dataset.ai]: b.dataset.v });
    if (e.target.closest('#ai-key-save')) { const k = el.querySelector('#ai-key').value.trim(); if (k) save({ anthropic_api_key: k }).then(() => toast('Key saved')) }
    if (e.target.closest('#ai-key-del')) save({ anthropic_api_key: '' }).then(() => toast('Key removed'));
    if (e.target.closest('#ai-check')) save({ local_url: el.querySelector('#ai-url').value.trim() }, true);
  };
  el.querySelector('#ai-key').onkeydown = e => { if (e.key === 'Enter') el.querySelector('#ai-key-save').click() };
  if (check || a.provider === 'local' || a.local_model) checkLocal();
}
