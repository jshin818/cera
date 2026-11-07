// Ask: a question in, an answer out, and every claim tied to the passage it came from. The server
// recalls the passages (vault-core) and streams the model's answer; this file renders the thread as
// one column of text, with each citation a pill that opens its passage in a modal.
import { api } from './api.js';
import { esc, ago, folderOf } from './util.js';

const MODELS = [['claude-opus-5-5', 'Opus 5.5'], ['claude-sonnet-5-5', 'Sonnet 5.5'], ['claude-haiku-5-5', 'Haiku 5.5']];
const modelName = id => MODELS.find(m => m[0] === id)?.[1] || id;
// Hosted OpenAI-compatible APIs offered by name; any other https URL works too.
const SERVICES = [['https://api.groq.com/openai/v1', 'Groq'], ['https://openrouter.ai/api/v1', 'OpenRouter'], ['https://api.together.xyz/v1', 'Together'], ['https://api.openai.com/v1', 'OpenAI'], ['https://api.mistral.ai/v1', 'Mistral'], ['https://api.deepseek.com/v1', 'DeepSeek']];
const serviceName = url => SERVICES.find(s => s[0] === url)?.[1] || (/^https?:\/\/(?:api\.)?([^/:]+)/.exec(url || '')?.[1] ?? 'API');
const X = '<svg class="ic" viewBox="0 0 24 24"><path d="M6 6l12 12M18 6 6 18"/></svg>';
const PREV = '<svg class="ic" viewBox="0 0 24 24"><path d="m15 6-6 6 6 6"/></svg>';
const NEXT = '<svg class="ic" viewBox="0 0 24 24"><path d="m9 6 6 6-6 6"/></svg>';

// The conversation outlives the view, so opening a source and coming back keeps it.
const thread = [];   // { q, recall, answer, model, note, error, state: 'recalling' | 'answering' | 'done' }
let ctrl = null;
let root = null;
let deps = null;
let aiState = null;

/* ---------- a small, safe Markdown renderer for answers ---------- */
// One pill per run of citations: "[2][3]" or "[2, 3]" reads as "Beta +1".
// Pills name the note; a leading date ("2026-10-14 Pricing review") is in the tooltip and the modal.
const short = t => t.replace(/^\d{4}-\d{2}-\d{2}\s+/, '') || t;
const pill = ps => `<button class="cite" data-n="${ps[0].n}" title="${esc(ps.map(p => `${p.n}. ${p.title}${p.section ? ' › ' + p.section : ''}`).join('\n'))}"><span class="cite-t">${esc(short(ps[0].title))}</span>${ps.length > 1 ? `<span class="cite-more">+${ps.length - 1}</span>` : ''}</button>`;
function inline(s, srcs) {
  return esc(s)
    .replace(/`([^`\n]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*\n]+)\*\*/g, '<b>$1</b>')
    .replace(/(^|[^*\w])\*([^*\n]+)\*(?!\w)/g, '$1<i>$2</i>')
    .replace(/\[\[([^\]|#\n]+)(?:#[^\]|\n]*)?(?:\|([^\]\n]+))?\]\]/g, (_, t, label) => `<button class="wl" data-link="${t}">${label || t}</button>`)
    .replace(/\s?\[\d{1,2}(?:,\s*\d{1,2})*\](?:\s?\[\d{1,2}(?:,\s*\d{1,2})*\])*/g, m => {
      const ps = [...new Set(m.match(/\d+/g).map(Number))].map(n => srcs?.find(x => x.n === n)).filter(Boolean);
      return ps.length ? '&nbsp;' + pill(ps) : m;
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
    <div id="ask-thread" aria-live="polite"></div>
    <div class="ask-dock">
      <div class="composer"><textarea id="ask-q" rows="1" placeholder="Ask about your notes" aria-label="Ask about your notes"></textarea><button id="ask-send" class="send" aria-label="Ask" title="Ask (↵)" disabled><svg class="ic" viewBox="0 0 24 24"><path d="M12 19V5M6 11l6-6 6 6"/></svg></button></div>
      <div class="ask-foot"><button id="ask-model" class="ask-model" title="Answer model — change in Settings"></button><span class="grow"></span><button id="ask-new" class="ask-link" hidden>New question</button></div>
    </div>
  </div>`;
  const q = ex.querySelector('#ask-q');
  const send = ex.querySelector('#ask-send');
  q.oninput = () => { send.disabled = !q.value.trim() };
  q.onkeydown = e => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); submit() } };
  send.onclick = submit;
  ex.querySelector('#ask-new').onclick = () => { ctrl?.abort(); thread.length = 0; draw(); q.focus() };
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
  const auto = a?.provider === 'auto';
  const apiReady = a?.has_api_key && a.api_model;
  if (live) label = live.provider === 'claude' ? `Claude ${modelName(live.model)}` : live.provider === 'api' ? `${serviceName(live.url)} · ${live.model}` : `Local · ${live.model}`;
  else if (!a || a.provider === 'off') label = 'Sources only';
  else if (a.provider === 'claude' || (auto && a.has_key)) label = a.has_key ? `Claude ${modelName(a.claude_model)}` : 'Claude · add a key';
  else if (a.provider === 'api' || (auto && apiReady)) label = `${serviceName(a.api_url)} · ${!a.has_api_key ? 'add a key' : a.api_model || 'pick a model'}`;
  else if (a.provider === 'local') label = `Local · ${a.local_model || 'first model'}`;
  else label = 'Local model or sources only';
  const ready = live || (a && (a.provider === 'local' || ((a.provider === 'claude' || auto) && a.has_key) || ((a.provider === 'api' || auto) && apiReady)));
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
  draw();
  let pending = false;
  const paint = () => { if (!pending) { pending = true; requestAnimationFrame(() => { pending = false; drawTurn(thread.indexOf(turn)) }) } };
  try {
    await api.ask({ question, history }, (ev, data) => {
      if (ev === 'sources') { turn.recall = data; turn.state = 'answering'; paint() }
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
    // The latest question sits at the top and its answer grows below it, as in a chat.
    const last = el.lastElementChild;
    el.scrollTop += last.getBoundingClientRect().top - el.getBoundingClientRect().top - 12;
  }
}

function drawTurn(i) {
  if (!alive() || i < 0) return;
  const t = thread[i];
  const el = root.querySelector(`.turn[data-t="${i}"]`);
  if (!el) return draw();
  const r = t.recall;
  const srcs = r?.passages || [];
  const n = srcs.length;
  let body;
  if (t.answer) body = `<div class="answer prose-a">${renderMarkdown(t.answer, srcs)}${t.state !== 'done' ? '<span class="caret"></span>' : ''}</div>`;
  else if (t.state === 'recalling') body = `<div class="answer muted"><span class="pulse"></span>Looking through your notes…</div>`;
  else if (t.state === 'answering') body = `<div class="answer muted"><span class="pulse"></span>Found ${n} ${n === 1 ? 'passage' : 'passages'}. Writing the answer…</div>`;
  else body = '';
  const sourcesOnly = t.state === 'done' && !t.answer && !t.error && n > 0;
  const done = t.state === 'done';
  // Gaps about the question stay in the thread; gaps about one source (stale, duplicate) live in its modal.
  const gaps = done ? r?.gaps.filter(g => g.kind !== 'stale' && g.kind !== 'duplicate') || [] : [];
  el.innerHTML = `<div class="q">${esc(t.q)}</div>
    ${body}
    ${sourcesOnly ? topPassages(r) : ''}
    ${t.note ? `<p class="a-quiet">${esc(t.note)}${/Settings/.test(t.note) ? ` <button class="ask-link" data-settings>Set up answers</button>` : ''}</p>` : ''}
    ${t.error ? `<p class="a-quiet err">${esc(t.error)}</p>` : ''}
    ${gaps.map(g => `<p class="a-quiet">${esc(g.message)}</p>`).join('')}
    ${done && n ? `<div class="a-meta"><button data-all>${n} ${n === 1 ? 'source' : 'sources'}</button>${t.answer ? `<button data-copy="${i}">Copy</button>` : ''}<button data-save="${i}">Save</button></div>` : ''}`;
}

// With no model, the best passages are the answer: quoted, each ending with its pill.
function topPassages(r) {
  const quote = p => renderMarkdown(clip(p.text, 280)).replace(/(<\/(?:p|li|h4|blockquote)>)((?:<\/[ou]l>)?)$/, `&nbsp;${pill([p])}$1$2`);
  return `<div class="answer prose-a">${r.passages.slice(0, 4).map(p => `<div class="quote">${quote(p)}</div>`).join('')}</div>`;
}
const clip = (s, n) => (s.length > n ? s.slice(0, n).replace(/\s+\S*$/, '') + '…' : s);

function onClick(e) {
  const b = e.target.closest('button');
  if (!b) return;
  const t = thread[+b.closest('.turn')?.dataset.t];
  if (b.matches('.cite')) return openSourceModal(t, +b.dataset.n, b);
  if (b.dataset.all !== undefined) return openSourceModal(t, null, b);
  if (b.dataset.path) return deps.openNote(b.dataset.path);
  if (b.dataset.link) return openLink(b.dataset.link);
  if (b.dataset.settings !== undefined) return deps.openSettings('ai');
  if (b.dataset.copy) return navigator.clipboard.writeText(thread[+b.dataset.copy].answer).then(() => deps.toast('Answer copied'), () => deps.toast('Copying is blocked here.'));
  if (b.dataset.save) return saveTurn(thread[+b.dataset.save]);
}
const openLink = name => api.resolve(name, '').then(r => r?.path ? deps.openNote(r.path) : deps.toast(`No note named “${name}”.`)).catch(() => {});

/* ---------- the source modal ---------- */
// Two views: the list of a turn's sources, and one passage with where it lives, why it was picked
// and how it links to the other sources. ← and → step through passages, ↵ opens the note there.
// A one-line preview of a passage: Markdown marks, wikilink brackets and [n] citations removed.
const plain = s => s.replace(/^#+\s.*\n/, '').replace(/\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g, (_, t, l) => l || t).replace(/\s?\[\d{1,2}\]|[*_`>#]+/g, '').replace(/^\s*[-•]\s+/gm, '').replace(/\s+/g, ' ').trim();
const whereOf = p => [folderOf(p.path), ...(p.section || '').split(' > ').filter(h => h && h !== p.title)].join(' › ') || p.path;

function openSourceModal(t, n, from) {
  const r = t?.recall;
  if (!r) return;
  let at = n == null ? null : Math.max(0, r.passages.findIndex(p => p.n === n));   // null: the list
  document.getElementById('src-veil')?.remove();
  const veil = document.createElement('div');
  veil.className = 'veil';
  veil.id = 'src-veil';
  veil.innerHTML = `<div class="pop src-modal" role="dialog" aria-modal="true" aria-labelledby="sm-title"></div>`;
  document.body.append(veil);
  const box = veil.firstChild;
  const titles = new Map(r.passages.map(p => [p.path, p.title]));
  const close = () => { veil.remove(); from?.focus() };
  const open = () => { const p = r.passages[at]; veil.remove(); deps.openSource(p) };
  const show = i => { const back = at; at = i; paint(); (i === null ? box.querySelector(`.sm-row[data-at="${back}"]`) || box.querySelector('.sm-row') : box).focus() };
  const step = d => { at = (at + d + r.passages.length) % r.passages.length; paint() };
  const closeBtn = `<button class="icon" data-close aria-label="Close sources" title="Close (esc)">${X}</button>`;
  const list = () => {
    const notes = new Set(r.passages.map(p => p.path)).size;
    return `<header class="sm-head">
        <div class="sm-name"><h2 id="sm-title">Sources</h2><div class="sm-where">${r.passages.length} ${r.passages.length === 1 ? 'passage' : 'passages'} from ${notes} ${notes === 1 ? 'note' : 'notes'}</div></div>
        ${closeBtn}
      </header>
      <div class="sm-body sm-list">${r.passages.map((p, i) => `<button class="sm-row" data-at="${i}">
          <span class="sm-n">${p.n}</span>
          <span class="sm-row-main"><b>${esc(p.title)}</b><small>${esc(whereOf(p))} · edited ${esc(ago(p.mtime_ms))}</small><span class="sm-snip">${esc(plain(p.text))}</span></span>
        </button>`).join('')}</div>`;
  };
  const passage = () => {
    const p = r.passages[at];
    const into = r.connections.filter(c => c.to === p.path && c.from !== p.path).map(c => c.from);
    const out = r.connections.filter(c => c.from === p.path && c.to !== p.path).map(c => c.to);
    const dups = r.gaps.filter(g => g.kind === 'duplicate' && g.path === p.path).map(g => g.message);
    const links = (label, paths) => paths.length ? `<div><span>${label}</span>${paths.map(x => `<button class="ask-link" data-path="${esc(x)}">${esc(titles.get(x) || x)}</button>`).join('')}</div>` : '';
    return `<header class="sm-head">
        <button class="icon sm-back" data-list aria-label="All sources" title="All sources">${PREV}</button>
        <div class="sm-name"><h2 id="sm-title">${esc(p.title)}</h2><div class="sm-where">${esc(whereOf(p))} · line ${p.line} · edited ${esc(ago(p.mtime_ms))}</div></div>
        ${closeBtn}
      </header>
      <div class="sm-body">
        <div class="sm-text prose-a">${renderMarkdown(p.text)}</div>
        ${p.why.length || dups.length ? `<p class="sm-quiet">${esc([...p.why, ...dups].join(' · '))}</p>` : ''}
        ${into.length || out.length ? `<div class="sm-links">${links('Linked from', into)}${links('Links to', out)}</div>` : ''}
      </div>
      <footer class="sm-foot">
        ${r.passages.length > 1 ? `<button class="icon" data-step="-1" aria-label="Previous source" title="Previous (←)">${PREV}</button><span class="sm-count">${at + 1} of ${r.passages.length}</span><button class="icon" data-step="1" aria-label="Next source" title="Next (→)">${NEXT}</button>` : ''}
        <span class="grow"></span>
        <button class="btn quiet" data-open>Open in note</button>
      </footer>`;
  };
  const paint = () => { box.innerHTML = at === null ? list() : passage() };
  veil.addEventListener('mousedown', e => { if (e.target === veil) close() });
  box.addEventListener('click', e => {
    const b = e.target.closest('button');
    if (!b) return;
    if (b.dataset.close !== undefined) return close();
    if (b.dataset.at) return show(+b.dataset.at);
    if (b.dataset.list !== undefined) return show(null);
    if (b.dataset.step) return step(+b.dataset.step);
    if (b.dataset.open !== undefined) return open();
    if (b.dataset.path) { veil.remove(); return deps.openNote(b.dataset.path) }
    if (b.dataset.link) { veil.remove(); return openLink(b.dataset.link) }
  });
  veil.addEventListener('keydown', e => {
    e.stopPropagation();
    if (e.key === 'Tab') {
      const f = [...box.querySelectorAll('button')];
      const i = f.indexOf(document.activeElement);
      if (e.shiftKey ? i <= 0 : i === f.length - 1) { e.preventDefault(); f[e.shiftKey ? f.length - 1 : 0].focus() }
    } else if (e.key === 'Escape') close();
    else if (at === null) {
      const rows = [...box.querySelectorAll('.sm-row')];
      const i = rows.indexOf(document.activeElement);
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); rows[Math.min(rows.length - 1, Math.max(0, i + (e.key === 'ArrowDown' ? 1 : -1)))]?.focus() }
    }
    else if (e.key === 'ArrowLeft') step(-1);
    else if (e.key === 'ArrowRight') step(1);
    else if (e.key === 'Backspace') show(null);
    else if (e.key === 'Enter' && e.target.matches('.src-modal, .icon[data-step]')) { e.preventDefault(); open() }
  });
  box.tabIndex = -1;
  show(at);
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
// Only the chosen provider's fields are shown; Auto shows all three, in the order it tries them.
let customApi = false;
export async function renderAiSettings(el, toast, check) {
  const a = (aiState = await api.ai().catch(() => null));
  if (!a) { el.innerHTML = `<p class="s-note">Couldn't load AI settings.</p>`; return }
  const seg = (key, value, options) => `<span class="seg" role="radiogroup">${options.map(([v, label]) => `<button role="radio" aria-checked="${v === value}" class="${v === value ? 'on' : ''}" data-ai="${key}" data-v="${v}">${label}</button>`).join('')}</span>`;
  const row = (label, hint, control) => `<div class="s-row"><div><b>${label}</b>${hint ? `<small>${hint}</small>` : ''}</div>${control}</div>`;
  const keyField = (id, label, has, removable, placeholder) => `<span class="s-field"><input id="${id}" type="password" autocomplete="off" spellcheck="false" placeholder="${has ? '••••••••' : placeholder}" aria-label="${label}">${removable ? `<button class="btn quiet" data-del="${id}">Remove</button>` : ''}<button class="btn quiet" data-put="${id}">Save</button></span>`;
  const show = k => a.provider === 'auto' || a.provider === k;
  const known = SERVICES.some(s => s[0] === a.api_url);
  const custom = customApi || !known;
  const keyHint = a.key_source === 'env' ? 'Using ANTHROPIC_API_KEY from the environment' : a.has_key ? 'Saved on this Mac, outside your vault' : 'From console.anthropic.com';
  el.innerHTML = `
    ${row('Answer with', a.provider === 'auto' ? 'Uses the first one that is set up' : '', seg('provider', a.provider, [['auto', 'Auto'], ['claude', 'Claude'], ['api', 'API'], ['local', 'Local'], ['off', 'Off']]))}
    ${show('claude') ? `<h4 class="s-sub">Claude</h4>
      ${row('API key', keyHint, keyField('ai-key', 'Claude API key', a.has_key, a.key_source === 'saved', 'sk-ant-…'))}
      ${row('Model', 'Opus is the most capable; Haiku is the fastest', seg('claude_model', a.claude_model, MODELS))}` : ''}
    ${show('api') ? `<h4 class="s-sub">API</h4>
      ${row('Service', 'Any OpenAI-compatible API', `<select id="ai-svc" aria-label="API service">${SERVICES.map(([u, n]) => `<option value="${u}"${!custom && u === a.api_url ? ' selected' : ''}>${n}</option>`).join('')}<option value=""${custom ? ' selected' : ''}>Other URL…</option></select>`)}
      ${custom ? row('URL', 'Up to the version, like https://…/v1', `<span class="s-field"><input id="ai-api-url" value="${known ? '' : esc(a.api_url)}" placeholder="https://" spellcheck="false" aria-label="API URL"><button class="btn quiet" data-put="ai-api-url">Save</button></span>`) : ''}
      ${row('API key', a.has_api_key ? 'Saved on this Mac, outside your vault' : `Your ${esc(serviceName(a.api_url))} key`, keyField('ai-api-key', 'API key', a.has_api_key, a.has_api_key, 'Paste key'))}
      <div id="ai-api"></div>` : ''}
    ${show('local') ? `<h4 class="s-sub">Local</h4>
      ${row('Server', 'Ollama: localhost:11434 · LM Studio: localhost:1234', `<span class="s-field"><input id="ai-url" value="${esc(a.local_url)}" spellcheck="false" aria-label="Local model server URL"><button class="btn quiet" id="ai-check">Check</button></span>`)}
      <div id="ai-local"></div>` : ''}
    <p class="s-note">Your question and the matching passages are sent to the model you pick: to Anthropic for Claude, to the service you chose for an API, or to the server on this Mac for a local model. Nothing is sent when answers are off.</p>`;
  const save = (patch, recheck) => api.setAi(patch).then(v => { aiState = v; drawModel(); return renderAiSettings(el, toast, recheck) }).catch(e => toast(e.message || String(e)));
  const models = (box, list, value, key, blank) => {
    box.innerHTML = row('Model', `${list.length} available`, `<select data-model="${key}" aria-label="Model">${['', ...list].map(m => `<option value="${esc(m)}"${m === value ? ' selected' : ''}>${m ? esc(m) : blank}</option>`).join('')}</select>`);
    box.querySelector('select').onchange = e => save({ [key]: e.target.value });
  };
  const checkLocal = async () => {
    const box = el.querySelector('#ai-local');
    if (!box) return;
    box.innerHTML = `<p class="s-note">Checking…</p>`;
    const r = await api.aiModels(el.querySelector('#ai-url').value.trim()).catch(() => null);
    if (!r || !r.reachable) { box.innerHTML = `<p class="s-note">Nothing answered there. Start Ollama or LM Studio, then check again.</p>`; return }
    if (!r.models.length) { box.innerHTML = `<p class="s-note">The server answered but has no models. Pull one first (for example <code>ollama pull llama3.2</code>).</p>`; return }
    models(box, r.models, a.local_model, 'local_model', 'First listed');
  };
  const checkApi = async () => {
    const box = el.querySelector('#ai-api');
    if (!box || !a.has_api_key) return;
    box.innerHTML = `<p class="s-note">Loading models…</p>`;
    const r = await api.aiModels(undefined, 'api').catch(e => ({ error: e.message }));
    if (!r.reachable) { box.innerHTML = `<p class="s-note">${esc(r.error || 'The API didn’t answer.')}</p>`; return }
    models(box, r.models, a.api_model, 'api_model', 'Pick a model');
  };
  const fields = { 'ai-key': 'anthropic_api_key', 'ai-api-key': 'api_key', 'ai-api-url': 'api_url' };
  el.onclick = e => {
    const b = e.target.closest('[data-ai]');
    if (b) return save({ [b.dataset.ai]: b.dataset.v });
    const put = e.target.closest('[data-put]');
    if (put) {
      const v = el.querySelector('#' + put.dataset.put).value.trim();
      if (v) save({ [fields[put.dataset.put]]: v }).then(() => { if (put.dataset.put !== 'ai-api-url') toast('Key saved'); else customApi = false });
    }
    const del = e.target.closest('[data-del]');
    if (del) save({ [fields[del.dataset.del]]: '' }).then(() => toast('Key removed'));
    if (e.target.closest('#ai-check')) save({ local_url: el.querySelector('#ai-url').value.trim() }, true);
  };
  el.querySelectorAll('input[type=password], #ai-api-url').forEach(i => { i.onkeydown = e => { if (e.key === 'Enter') i.parentElement.querySelector('[data-put]').click() } });
  const svc = el.querySelector('#ai-svc');
  if (svc) svc.onchange = () => { customApi = !svc.value; if (svc.value) save({ api_url: svc.value }); else renderAiSettings(el, toast) };
  if (check || a.provider === 'local' || a.local_model) checkLocal();
  checkApi();
}
