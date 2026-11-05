// Cēra — the desktop UI over the Vault engine. Vanilla JS: the engine does the heavy lifting
// (index, search, discovery) server-side; this file renders and sends commands.
import { api } from './api.js';
import { createEditor } from './editor.js';
import { mountGraph } from './graph.js';
import { $, esc, tagColor, debounce, SVG, ICON, logo, ago, titleOf, folderOf, fmt } from './util.js';

const app = $('app');
const store = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v) } catch { return d } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)) } catch {} },
};
const mobile = () => matchMedia('(max-width: 768px)').matches;

/* ---------- state ---------- */
let ws = { tabs: [], active: 0, last: null, openFolder: null };
let cur = null;            // { path, hash, title, tags, aliases, words, backlinks, mtime }
let dirty = false;
let saving = null;
let view = 'home';         // home | page | explore
let exView = null;         // { kind, arg }
let status = {};
const dirCache = new Map(); // folder path -> listing
const openFolders = new Set(store.get('cera.openFolders', []));
let tagsCache = null;
let vaultName = 'All notes';

/* ---------- toast ---------- */
let toastT;
function toast(msg, action, fn, ms = 5000) {
  const t = $('toast');
  t.textContent = msg;
  t.hidden = false;
  if (action) { const b = document.createElement('button'); b.textContent = action; b.onclick = () => { t.hidden = true; fn() }; t.append(b) }
  clearTimeout(toastT);
  toastT = setTimeout(() => (t.hidden = true), ms);
}
const fail = e => toast(e?.message || String(e));

/* ---------- logo / theme ---------- */
document.querySelectorAll('[data-logo]').forEach(el => (el.innerHTML = logo(+el.dataset.logo)));
const darkQuery = matchMedia('(prefers-color-scheme: dark)');
let theme = store.get('cera.theme', 'system');
const effectiveTheme = () => (theme !== 'system' ? theme : darkQuery.matches ? 'dark' : 'light');
function applyTheme() {
  if (theme !== 'system') document.documentElement.dataset.theme = theme; else delete document.documentElement.dataset.theme;
  const dark = effectiveTheme() === 'dark';
  const b = $('theme-btn');
  b.innerHTML = SVG(dark ? ICON.sun : ICON.moon);
  b.title = dark ? 'Switch to light mode' : 'Switch to dark mode';
  b.setAttribute('aria-label', b.title);
}
function setTheme(t) {
  if (t === theme) return;
  const root = document.documentElement;
  if (!matchMedia('(prefers-reduced-motion: reduce)').matches) { root.classList.add('theme-fade'); setTimeout(() => root.classList.remove('theme-fade'), 220) }
  theme = t;
  try { localStorage.setItem('cera.theme', t) } catch {}
  applyTheme();
  if (!$('settings-veil').hidden) renderSettings();
}
$('theme-btn').onclick = () => setTheme(effectiveTheme() === 'dark' ? 'light' : 'dark');
darkQuery.addEventListener('change', applyTheme);
applyTheme();

/* ---------- sidebar ---------- */
function setSide(open) {
  app.classList.toggle('collapsed', !open);
  $('open-side').hidden = open;
  $('scrim').hidden = !(open && mobile());
  if (!mobile()) store.set('cera.side', open);
}
$('open-side').onclick = () => setSide(true);
$('close-side').onclick = () => setSide(false);
$('scrim').onclick = () => setSide(false);

let sideTab = store.get('cera.sideTab', 'notes');
function setSideTab(t) {
  sideTab = t;
  store.set('cera.sideTab', t);
  $('tab-notes').classList.toggle('on', t === 'notes');
  $('tab-explore').classList.toggle('on', t === 'explore');
  $('tab-notes').setAttribute('aria-selected', t === 'notes');
  $('tab-explore').setAttribute('aria-selected', t === 'explore');
  $('notes-pane').hidden = t !== 'notes';
  $('ex-nav').hidden = t !== 'explore';
  if (t === 'explore') renderExNav();
}
$('tab-notes').onclick = () => setSideTab('notes');
$('tab-explore').onclick = () => setSideTab('explore');

async function listing(path, fresh) {
  if (!fresh && dirCache.has(path)) return dirCache.get(path);
  const l = await api.dir(path);
  dirCache.set(path, l);
  return l;
}

const ROW_H = 31;
// Long folders (thousands of notes) render as a windowed list so the sidebar stays light.
function noteList(notes, depthClass = '') {
  const wrap = document.createElement('div');
  wrap.className = 'f-notes ' + depthClass;
  const row = n => `<button class="note-link${n.markdown ? '' : ' file'}${cur?.path === n.path ? ' on' : ''}" data-path="${esc(n.path)}" draggable="${n.markdown}" title="${esc(n.path)}">${esc(n.markdown ? n.title : n.path.split('/').pop())}</button>`;
  if (notes.length <= 200) {
    wrap.innerHTML = notes.map(row).join('');
    return wrap;
  }
  const box = document.createElement('div');
  box.className = 'vlist';
  box.style.height = Math.min(notes.length * ROW_H, window.innerHeight * 0.6) + 'px';
  const inner = document.createElement('div');
  inner.style.height = notes.length * ROW_H + 'px';
  inner.style.position = 'relative';
  box.append(inner);
  const draw = () => {
    const first = Math.max(0, Math.floor(box.scrollTop / ROW_H) - 10);
    const last = Math.min(notes.length, first + Math.ceil(box.clientHeight / ROW_H) + 20);
    inner.innerHTML = `<div style="position:absolute;left:0;right:0;top:${first * ROW_H}px">${notes.slice(first, last).map(row).join('')}</div>`;
  };
  box.addEventListener('scroll', () => requestAnimationFrame(draw), { passive: true });
  requestAnimationFrame(draw);
  wrap.append(box);
  return wrap;
}

function folderRow(d, depth) {
  const open = openFolders.has(d.path);
  return `<div class="f-row${open ? ' open' : ''}" data-folder="${esc(d.path)}" draggable="true" role="treeitem" aria-expanded="${open}" tabindex="0" style="padding-left:${6 + depth * 12}px">
    ${SVG(ICON.chev, 'ic chev')}${SVG(ICON.folder, 'ic fold')}<span class="f-name">${esc(d.name)}</span>
    <span class="f-count">${d.notes ? fmt(d.notes) : ''}</span>
    <button class="f-act" data-act="more" aria-label="Folder actions" title="More">${SVG(ICON.more)}</button></div>`;
}

async function renderFolder(container, path, depth) {
  const l = await listing(path);
  container.innerHTML = '';
  for (const d of l.dirs) {
    const holder = document.createElement('div');
    holder.innerHTML = folderRow(d, depth);
    container.append(holder);
    if (openFolders.has(d.path)) {
      const kids = document.createElement('div');
      kids.className = 'f-kids';
      holder.append(kids);
      renderFolder(kids, d.path, depth + 1);
    }
  }
  // One tree: every level, the top level included, lists its folders first and then its notes.
  if (!l.notes.length && !l.dirs.length) {
    const e = document.createElement('div');
    e.className = 'f-empty';
    e.style.paddingLeft = 22 + depth * 12 + 'px';
    e.textContent = depth ? 'Empty' : 'No notes yet';
    container.append(e);
  } else if (l.notes.length) {
    const nl = noteList(l.notes);
    nl.style.paddingLeft = 10 + depth * 12 + 'px';
    container.append(nl);
  }
}

async function renderSidebar(fresh) {
  if (fresh) dirCache.clear();
  try { await renderFolder($('f-list'), '', 0) } catch (e) { /* engine still starting */ }
}
const refreshSidebar = debounce(() => renderSidebar(true), 250);

function markActiveInSidebar() {
  document.querySelectorAll('#side .note-link.on').forEach(b => b.classList.remove('on'));
  if (cur) document.querySelectorAll(`#side .note-link[data-path="${CSS.escape(cur.path)}"]`).forEach(b => b.classList.add('on'));
}

$('side-scroll').addEventListener('click', async e => {
  if (e.target.closest('#f-edit')) return; // typing a folder name, not navigating
  const note = e.target.closest('.note-link[data-path]');
  if (note) { openNote(note.dataset.path, { newTab: e.metaKey || e.ctrlKey }); if (mobile()) setSide(false); return }
  const act = e.target.closest('[data-act="more"]');
  const row = e.target.closest('.f-row[data-folder]');
  if (act && row) { e.stopPropagation(); return folderMenu(row.dataset.folder, act) }
  if (row) toggleFolder(row.dataset.folder);
});
$('side-scroll').addEventListener('keydown', e => {
  const row = e.target.closest('.f-row');
  if (row?.dataset.folder !== undefined && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); toggleFolder(row.dataset.folder) }
});

function toggleFolder(path) {
  if (path === undefined) return;
  if (openFolders.has(path)) {
    [...openFolders].filter(p => p === path || p.startsWith(path + '/')).forEach(p => openFolders.delete(p));
  } else {
    // Only one main folder open at a time; nested folders open freely inside it.
    if (!path.includes('/')) [...openFolders].filter(p => !p.startsWith(path + '/')).forEach(p => openFolders.delete(p));
    openFolders.add(path);
  }
  store.set('cera.openFolders', [...openFolders]);
  renderSidebar(false);
}

/* drag and drop: notes and folders move onto a folder, into an open folder's contents, or onto
   empty space in the tree (the top level). Hovering a closed folder while dragging opens it. */
const NOTE_T = 'text/x-cera-path', FOLDER_T = 'text/x-cera-folder';
let dragging = null;     // { kind: 'note' | 'folder', path }
let dropEl = null;
let springT = null;
function dropTargetAt(el) {
  const row = el.closest('.f-row[data-folder]');
  if (row) return { el: row, folder: row.dataset.folder };
  const kids = el.closest('.f-kids');
  if (kids) { const r = kids.parentElement.querySelector(':scope > .f-row'); return { el: r, folder: r.dataset.folder } }
  return { el: $('notes-pane'), folder: '' };
}
function canDrop(folder) {
  if (!dragging) return false;
  const p = dragging.path;
  if (dragging.kind === 'folder') return folder !== p && !folder.startsWith(p + '/') && folder !== folderOf(p);
  return folder !== folderOf(p);
}
function setDropEl(el) {
  if (dropEl === el) return;
  dropEl?.classList.remove('drop');
  dropEl = el;
  el?.classList.add('drop');
  clearTimeout(springT);
  const f = el?.dataset.folder;
  if (f && !openFolders.has(f)) springT = setTimeout(() => { if (dropEl === el) toggleFolder(f) }, 650);
}
function endDrag() {
  setDropEl(null);
  document.querySelector('.dragging')?.classList.remove('dragging');
  $('notes-pane').classList.remove('dragging-in');
  dragging = null;
}
$('side-scroll').addEventListener('dragstart', e => {
  const n = e.target.closest('.note-link[data-path]');
  const f = !n && e.target.closest('.f-row[data-folder]');
  if (!n && !f) return;
  dragging = n ? { kind: 'note', path: n.dataset.path } : { kind: 'folder', path: f.dataset.folder };
  e.dataTransfer.effectAllowed = 'move';
  e.dataTransfer.setData(n ? NOTE_T : FOLDER_T, dragging.path);
  e.dataTransfer.setData('text/plain', n ? titleOf(dragging.path) : dragging.path.split('/').pop());
  (n || f).classList.add('dragging');
  $('notes-pane').classList.add('dragging-in');
});
$('side-scroll').addEventListener('dragover', e => {
  if (!dragging) return;
  const t = dropTargetAt(e.target);
  if (!canDrop(t.folder)) { e.dataTransfer.dropEffect = 'none'; setDropEl(null); return }
  e.preventDefault();
  e.dataTransfer.dropEffect = 'move';
  setDropEl(t.el);
});
$('side-scroll').addEventListener('dragleave', e => { if (!$('side-scroll').contains(e.relatedTarget)) setDropEl(null) });
$('side-scroll').addEventListener('dragend', endDrag);
$('side-scroll').addEventListener('drop', e => {
  if (!dragging) return;
  e.preventDefault();
  const t = dropTargetAt(e.target);
  const d = dragging;
  const ok = canDrop(t.folder);
  endDrag();
  if (!ok) return;
  if (d.kind === 'note') moveNote(d.path, t.folder); else moveFolder(d.path, t.folder);
});

async function moveFolder(path, into) {
  const name = path.split('/').pop();
  const to = into ? `${into}/${name}` : name;
  try {
    const r = await api.renameFolder(path, to);
    retargetFolder(path, r.path);
    if (into) openFolders.add(into);
    store.set('cera.openFolders', [...openFolders]);
    refreshSidebar();
    toast(`Moved “${name}” to ${into ? into.split('/').pop() : 'the top level'}`);
  } catch (e) { fail(e) }
}
// After a folder moves or is renamed, keep open folders, tabs and the open note pointing at it.
function retargetFolder(from, to) {
  const swap = p => (p === from || p.startsWith(from + '/') ? to + p.slice(from.length) : p);
  for (const p of [...openFolders]) { const q = swap(p); if (q !== p) { openFolders.delete(p); openFolders.add(q) } }
  ws.tabs = ws.tabs.map(swap);
  if (ws.last) ws.last = swap(ws.last);
  saveWs();
  renderTabs();
  if (cur) { const q = swap(cur.path); if (q !== cur.path) { cur.path = q; renderCrumb() } }
}

async function moveNote(path, folder) {
  const name = path.split('/').pop();
  const to = folder ? `${folder}/${name}` : name;
  if (to === path) return;
  try {
    const r = await api.rename(path, to);
    retargetTabs(path, r.path);
    if (cur?.path === path) { cur.path = r.path; renderCrumb() }
    refreshSidebar();
    toast(`Moved to ${folder ? folder.split('/').pop() : 'the top level'}`);
  } catch (e) { fail(e) }
}

/* folder menu */
let menuFolder = null;
function place(pop, anchor) {
  const r = anchor.getBoundingClientRect();
  pop.hidden = false;
  const w = pop.offsetWidth, h = pop.offsetHeight;
  pop.style.left = Math.max(8, Math.min(r.left, innerWidth - w - 8)) + 'px';
  pop.style.top = (r.bottom + 4 + h > innerHeight ? Math.max(8, r.top - h - 4) : r.bottom + 4) + 'px';
}
function folderMenu(path, anchor) {
  menuFolder = path;
  place($('fmenu'), anchor);
}
$('fm-note').onclick = () => { $('fmenu').hidden = true; newNote(menuFolder) };
$('fm-sub').onclick = () => { $('fmenu').hidden = true; inlineFolderName(menuFolder, '', async name => { await api.mkdir(`${menuFolder}/${name}`); openFolders.add(menuFolder) }) };
$('fm-rename').onclick = () => {
  $('fmenu').hidden = true;
  const parent = folderOf(menuFolder);
  inlineFolderName(parent, menuFolder.split('/').pop(), async name => {
    const to = parent ? `${parent}/${name}` : name;
    const from = menuFolder;
    await api.renameFolder(from, to);
    retargetFolder(from, to);
    store.set('cera.openFolders', [...openFolders]);
  }, menuFolder);
};
$('fm-del').onclick = async () => {
  $('fmenu').hidden = true;
  const p = menuFolder;
  try { await api.delFolder(p); toast(`Moved “${p.split('/').pop()}” to the vault's .trash folder`); refreshSidebar() } catch (e) { fail(e) }
};
$('f-add').onclick = () => inlineFolderName('', '', name => api.mkdir(name));

// Inline name editor that appears in the folder list.
function inlineFolderName(parent, initial, done, replacing) {
  const row = document.createElement('div');
  row.className = 'f-row';
  row.innerHTML = `${SVG(ICON.folder, 'ic fold')}<input id="f-edit" maxlength="120" aria-label="Folder name">`;
  const target = replacing ? document.querySelector(`.f-row[data-folder="${CSS.escape(replacing)}"]`) : null;
  if (target) target.replaceWith(row); else $('f-list').prepend(row);
  const inp = row.querySelector('input');
  inp.value = initial;
  inp.focus();
  inp.select();
  let finished = false;
  const finish = async ok => {
    if (finished) return;
    finished = true;
    const name = inp.value.trim().replace(/[\\/:*?"<>|]/g, ' ').trim();
    if (ok && name && name !== initial) { try { await done(name) } catch (e) { fail(e) } }
    refreshSidebar();
  };
  inp.onkeydown = e => { if (e.key === 'Enter') finish(true); if (e.key === 'Escape') finish(false) };
  inp.onblur = () => finish(true);
}

/* ---------- explore nav ---------- */
function renderExNav() {
  const items = [
    ['search', ICON.search, 'Search', ''],
    ['tags', ICON.tag, 'Tags', ''],
    ['topics', ICON.topics, 'Topics', status.clusters ? fmt(status.clusters) : ''],
    ['graph', ICON.graph, 'Graph', ''],
    ['data', ICON.data, 'Data', ''],
  ];
  $('ex-nav').innerHTML = items.map(([k, ic, label, n]) => `<button class="ex-row${view === 'explore' && exView?.kind === k ? ' on' : ''}" data-ex="${k}">${SVG(ic)}<span class="f-name">${label}</span><small>${n}</small></button>`).join('');
}
$('ex-nav').addEventListener('click', e => { const b = e.target.closest('[data-ex]'); if (b) explore(b.dataset.ex) });

/* ---------- views ---------- */
function show(v) {
  view = v;
  $('home').hidden = v !== 'home';
  $('page').hidden = v !== 'page';
  $('explore').hidden = v !== 'explore';
  $('more').hidden = v !== 'page';
  $('graph-btn').hidden = v !== 'page';
  $('crumb').hidden = v !== 'page';
  if (v !== 'explore') $('explore').classList.remove('wide');
  renderTabs();
  if (sideTab === 'explore') renderExNav();
}

function goHome() {
  flush();
  cur = null;
  show('home');
  $('capture').value = '';
  renderHome();
  markActiveInSidebar();
  if (!mobile()) $('capture').focus();
}
$('brand').onclick = goHome;

/* ---------- home ---------- */
let homeRows = [];
let homeSel = -1;
async function renderHome() {
  const q = $('capture').value.trim();
  $('send').disabled = !q;
  try {
    if (!q) {
      const [freq, recent] = await Promise.all([api.switch('', 6), api.recent(8)]);
      const seen = new Set();
      homeRows = [...freq, ...recent].filter(n => !seen.has(n.path) && seen.add(n.path)).slice(0, 7).map(n => ({ path: n.path, title: n.title, sub: n.mtime_ms ? `Edited ${ago(n.mtime_ms)}` : folderOf(n.path) }));
      $('matches').innerHTML = homeRows.length
        ? `<div class="group">Jump back in</div>` + homeRows.map((r, i) => hitHtml(r, i)).join('')
        : `<p class="hint">Write a thought and press ↵ to save it as a note. Your Markdown files stay plain files on disk.</p>`;
    } else {
      const [sw, se] = await Promise.all([api.switch(q, 5), api.search(q, 'hybrid', 6).catch(() => ({ results: [] }))]);
      const seen = new Set(sw.map(s => s.path));
      homeRows = [
        ...sw.map(s => ({ path: s.path, title: s.title, sub: s.alias ? `alias: ${s.alias}` : folderOf(s.path) })),
        ...se.results.filter(r => !seen.has(r.path)).map(r => ({ path: r.path, title: r.title, snippet: r.snippet, sub: r.section || folderOf(r.path) })),
      ];
      $('matches').innerHTML = homeRows.map((r, i) => hitHtml(r, i)).join('') + `<p class="hint">↵ saves “${esc(firstLine(q))}” as a new note · ↑↓ to pick a match</p>`;
    }
    homeSel = -1;
  } catch { /* still indexing */ }
}
const firstLine = s => s.split('\n')[0].slice(0, 80);
function hitHtml(r, i) {
  return `<button class="hit" data-i="${i}" data-path="${esc(r.path)}"><b>${esc(r.title)}<span class="where">${esc(r.sub || '')}</span></b>${r.snippet ? `<small>${r.snippet}</small>` : ''}</button>`;
}
const homeSearch = debounce(renderHome, 90);
$('capture').addEventListener('input', homeSearch);
$('capture').addEventListener('keydown', e => {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    homeSel = Math.max(-1, Math.min(homeRows.length - 1, homeSel + (e.key === 'ArrowDown' ? 1 : -1)));
    document.querySelectorAll('#matches .hit').forEach((b, i) => b.classList.toggle('sel', i === homeSel));
  } else if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    if (homeSel >= 0 && homeRows[homeSel]) openNote(homeRows[homeSel].path, { newTab: e.metaKey || e.ctrlKey });
    else capture();
  }
});
$('send').onclick = capture;
$('matches').addEventListener('click', e => { const h = e.target.closest('.hit[data-path]'); if (h) openNote(h.dataset.path, { newTab: e.metaKey || e.ctrlKey }) });

async function capture() {
  const text = $('capture').value.trim();
  if (!text) return;
  const [first, ...rest] = text.split('\n');
  const title = first.replace(/^#+\s*/, '').slice(0, 100);
  const body = rest.join('\n').trim();
  try {
    const r = await api.create('', title, body ? body + '\n' : '');
    $('capture').value = '';
    refreshSidebar();
    openNote(r.path, { focusEnd: true });
  } catch (e) { fail(e) }
}

/* ---------- editor ---------- */
const excerptCache = new Map();
const editor = createEditor($('ed'), {
  title: () => cur?.title,
  onChange: () => { dirty = true; scheduleSave(); writingMode(); refreshChips() },
  resolve: async target => {
    try { return (await api.resolve(target, cur?.path || '')).path } catch { return null }
  },
  excerpt: async (path, heading) => {
    const k = path + '#' + (heading || '');
    if (excerptCache.has(k)) return excerptCache.get(k);
    try {
      const n = await api.note(path);
      let body = n.content.replace(/^---\n[\s\S]*?\n---\n?/, '');
      if (heading) {
        const i = body.toLowerCase().indexOf('# ' + heading.toLowerCase());
        if (i >= 0) body = body.slice(body.indexOf('\n', i) + 1);
      }
      const text = body.replace(/^#+\s+/gm, '').trim().slice(0, 600);
      excerptCache.set(k, text);
      return text;
    } catch { return '' }
  },
  searchTitles: async q => {
    try { return (await api.switch(q, 12)).map(h => ({ title: h.title, alias: h.alias, folder: folderOf(h.path) })) } catch { return [] }
  },
  tags: async () => (tagsCache ||= await api.tags().catch(() => [])),
  openLink: async (target, newTab) => {
    const t = target.split('|')[0];
    const [name, sub] = t.split(/#(.*)/s);
    if (!name.trim()) return; // same-note heading link
    flush();
    let path = (await api.resolve(name, cur?.path || '').catch(() => ({}))).path;
    if (!path) {
      // Obsidian behaviour: following a link to a missing note creates it.
      const r = await api.create(folderOf(cur?.path || ''), titleOf(name), '').catch(fail);
      if (!r) return;
      path = r.path;
      refreshSidebar();
    }
    if (/\.md$/i.test(path)) openNote(path, { newTab, heading: sub?.replace(/^\^/, '') });
    else window.open('/file/' + path.split('/').map(encodeURIComponent).join('/'), '_blank');
  },
  openTag: tag => explore('tags', tag),
  upload: async file => {
    if (!cur) return null;
    try { return await api.upload(cur.path, file) } catch (e) { fail(e); return null }
  },
});

let lastSuggested = [];
const refreshChips = debounce(() => renderTags(lastSuggested), 300);
let saveT;
function scheduleSave() {
  clearTimeout(saveT);
  setStatus('Editing');
  saveT = setTimeout(save, 450);
}
async function save(force) {
  clearTimeout(saveT);
  if (!cur || !dirty) return;
  const path = cur.path;
  const content = editor.text;
  dirty = false;
  setStatus('Saving…');
  const p = api.save(path, content, force ? undefined : cur.hash).then(r => {
    if (cur?.path === path) { cur.hash = r.hash; cur.mtime = Date.now() }
    setStatus('Saved');
    tagsCache = null;
    loadBelow(path);
  }).catch(e => {
    if (e.status === 409 && e.kind === 'conflict') {
      dirty = true;
      toast('This note changed on disk.', 'Keep mine', () => save(true), 20000);
      setTimeout(() => { if ($('toast').hidden === false) return; }, 0);
      conflictPrompt(path);
    } else { dirty = true; fail(e) }
  });
  saving = p;
  await p;
}
function conflictPrompt(path) {
  const t = $('toast');
  const b = document.createElement('button');
  b.textContent = 'Load theirs';
  b.onclick = async () => { t.hidden = true; dirty = false; await openNote(path, { replace: true, reload: true }) };
  t.append(b);
}
function flush() { if (dirty) save() }
let statusT;
function setStatus(s) {
  $('status').textContent = s;
  clearTimeout(statusT);
  if (s === 'Saved') statusT = setTimeout(() => ($('status').textContent = ''), 1500);
}

let writingT;
function writingMode() {
  if (!editor.hasFocus()) return;
  document.body.classList.add('writing');
  clearTimeout(writingT);
  writingT = setTimeout(() => document.body.classList.remove('writing'), 1600);
}
addEventListener('mousemove', () => { if (document.body.classList.contains('writing')) { clearTimeout(writingT); document.body.classList.remove('writing') } });

/* ---------- open a note ---------- */
let openSeq = 0;
async function openNote(path, opts = {}) {
  if (!path) return;
  flush();
  const seq = ++openSeq;
  let n;
  try { n = await api.note(path) } catch (e) {
    if (e.status === 404) { toast('That note no longer exists.'); closeTabByPath(path); return }
    return fail(e);
  }
  if (seq !== openSeq) return;
  // tabs
  const idx = ws.tabs.indexOf(n.path);
  if (opts.newTab && idx < 0) { ws.tabs.splice(ws.active + 1, 0, n.path); ws.active = ws.active + 1 }
  else if (idx >= 0) ws.active = idx;
  else if (!ws.tabs.length) { ws.tabs = [n.path]; ws.active = 0 }
  else ws.tabs[ws.active] = n.path;
  ws.last = n.path;
  saveWs();
  cur = { path: n.path, hash: n.hash, title: n.title, tags: n.tags, aliases: n.aliases, words: n.words, backlinks: n.backlinks, mtime: n.mtime_ms };
  dirty = false;
  excerptCache.clear();
  show('page');
  $('title').value = n.title;
  editor.set(n.content, { keepSelection: !!opts.reload });
  renderMeta();
  renderTags([]);
  renderCrumb();
  markActiveInSidebar();
  if (!opts.reload) $('scroll').scrollTop = 0;
  if (opts.heading) jumpToHeading(n.content, opts.heading);
  else if (opts.focusEnd) editor.focus(true);
  else if (opts.pos != null) editor.reveal(opts.pos);
  loadBelow(n.path);
  document.title = `${n.title} — Cera`;
}

function jumpToHeading(content, h) {
  const lines = content.split('\n');
  let pos = 0;
  for (const l of lines) {
    const m = /^#{1,6}\s+(.*)$/.exec(l);
    if ((m && m[1].trim().toLowerCase() === h.toLowerCase()) || l.endsWith(' ^' + h)) return editor.reveal(pos);
    pos += l.length + 1;
  }
}

function renderMeta() {
  if (!cur) return;
  const bits = [];
  if (cur.mtime) bits.push(`Edited ${ago(cur.mtime)}`);
  if (cur.words) bits.push(`${fmt(cur.words)} words`);
  if (cur.backlinks) bits.push(`${cur.backlinks} ${cur.backlinks === 1 ? 'backlink' : 'backlinks'}`);
  if (cur.aliases?.length) bits.push(`also ${cur.aliases.join(', ')}`);
  $('meta').textContent = bits.join(' · ');
}
setInterval(() => { if (view === 'page' && !dirty) renderMeta() }, 60_000);

function renderCrumb() {
  if (!cur) return;
  const f = folderOf(cur.path);
  $('crumb').innerHTML = `${SVG(ICON.folder)}<span>${esc(f ? f.split('/').pop() : 'Notes')}</span>`;
  $('crumb').title = f ? `In ${f} — move to another folder` : 'Move to a folder';
}

/* title = file name (Obsidian semantics): renaming the title renames the file and updates links */
$('title').addEventListener('keydown', e => {
  if (e.key === 'Enter') { e.preventDefault(); editor.focus() }
  if (e.key === 'Escape') { $('title').value = cur?.title || ''; editor.focus() }
});
$('title').addEventListener('blur', renameFromTitle);
async function renameFromTitle() {
  if (!cur) return;
  const t = $('title').value.replace(/[\\/:*?"<>|#^[\]]/g, ' ').replace(/\s+/g, ' ').trim();
  if (!t || t === cur.title) { $('title').value = cur.title; return }
  flush();
  const f = folderOf(cur.path);
  const to = (f ? f + '/' : '') + t + '.md';
  try {
    const from = cur.path;
    const r = await api.rename(from, to);
    retargetTabs(from, r.path);
    cur.path = r.path;
    cur.title = titleOf(r.path);
    $('title').value = cur.title;
    document.title = `${cur.title} — Cera`;
    refreshSidebar();
    toast('Renamed. Links to this note were updated.');
  } catch (e) { $('title').value = cur.title; fail(e) }
}

/* move (crumb) */
$('crumb').onclick = async e => {
  e.stopPropagation();
  const pop = $('move');
  const all = await api.dir('').catch(() => ({ dirs: [] }));
  const dirs = [''];
  const walk = async (list, depth) => { for (const d of list) { dirs.push(d.path); if (depth < 2) { const l = await listing(d.path); await walk(l.dirs, depth + 1) } } };
  await walk(all.dirs, 0);
  const render = q => dirs.filter(d => d.toLowerCase().includes(q.toLowerCase())).slice(0, 80).map(d => `<button class="row" data-dir="${esc(d)}">${SVG(ICON.folder)}${esc(d || 'Top level')}</button>`).join('');
  pop.innerHTML = `<input placeholder="Move to…" aria-label="Filter folders"><div class="list">${render('')}</div>`;
  place(pop, $('crumb'));
  const inp = pop.querySelector('input');
  inp.focus();
  inp.oninput = () => (pop.querySelector('.list').innerHTML = render(inp.value));
  pop.onclick = ev => { const b = ev.target.closest('[data-dir]'); if (b) { pop.hidden = true; moveNote(cur.path, b.dataset.dir) } };
};

/* ---------- tags (frontmatter) ---------- */
function splitFrontmatter(text) {
  const m = /^---\n([\s\S]*?)\n---\n?/.exec(text);
  return m ? { yaml: m[1], body: text.slice(m[0].length) } : { yaml: null, body: text };
}
function fmTags(yaml) {
  if (!yaml) return [];
  const inline = /^tags?:\s*\[(.*)\]\s*$/m.exec(yaml);
  if (inline) return inline[1].split(',').map(s => s.trim().replace(/^["']|["']$/g, '').replace(/^#/, '')).filter(Boolean);
  const single = /^tags?:\s*(\S.*)$/m.exec(yaml);
  if (single) return single[1].split(/[\s,]+/).map(s => s.replace(/^#/, '')).filter(Boolean);
  const block = /^tags?:\s*\n((?:\s+-\s+.*\n?)+)/m.exec(yaml);
  if (block) return block[1].split('\n').map(l => l.replace(/^\s+-\s+/, '').trim().replace(/^["']|["']$/g, '')).filter(Boolean);
  return [];
}
function withTags(text, tags) {
  const { yaml, body } = splitFrontmatter(text);
  const line = tags.length ? `tags: [${tags.join(', ')}]` : '';
  let y = yaml ?? '';
  y = y.replace(/^tags?:\s*\[.*\]\s*$/m, '\u0000').replace(/^tags?:\s*\n(?:\s+-\s+.*\n?)+/m, '\u0000\n').replace(/^tags?:\s*\S.*$/m, '\u0000');
  if (y.includes('\u0000')) y = y.replace('\u0000', line).replace(/\n\n+/g, '\n');
  else if (line) y = y ? y + '\n' + line : line;
  y = y.split('\n').filter(l => l.trim() !== '').join('\n');
  return y ? `---\n${y}\n---\n${body}` : body;
}
function renderTags(suggested) {
  if (!cur) return;
  const { yaml, body } = splitFrontmatter(editor.text);
  const own = fmTags(yaml).map(t => t.toLowerCase());
  // Inline #tags, read from the live text (code blocks and inline code excluded).
  const prose = body.replace(/```[\s\S]*?```/g, '').replace(/`[^`\n]*`/g, '');
  const inline = [...new Set([...prose.matchAll(/(?:^|[\s(])#([\p{L}\p{N}_\-/]*[\p{L}_\-/][\p{L}\p{N}_\-/]*)/gu)].map(m => m[1].toLowerCase()))].filter(t => !own.includes(t));
  $('tag-chips').innerHTML =
    own.map(t => `<span class="tag-chip" data-c="${tagColor(t)}"><button data-tag="${esc(t)}">${esc(t)}</button><button data-rm="${esc(t)}" aria-label="Remove tag ${esc(t)}">×</button></span>`).join('') +
    inline.map(t => `<button class="tag-chip" data-c="${tagColor(t)}" data-tag="${esc(t)}" title="Inline tag in the text">${esc(t)}</button>`).join('') +
    suggested.filter(s => !own.includes(s.tag)).map(s => `<button class="tag-chip suggest" data-c="${tagColor(s.tag)}" data-add="${esc(s.tag)}" title="Suggested from similar notes">+ ${esc(s.tag)}</button>`).join('');
}
$('tags').addEventListener('click', e => {
  const t = e.target.closest('[data-tag]');
  if (t) return explore('tags', t.dataset.tag);
  const rm = e.target.closest('[data-rm]');
  if (rm) return setTags(fmTags(splitFrontmatter(editor.text).yaml).filter(x => x.toLowerCase() !== rm.dataset.rm));
  const add = e.target.closest('[data-add]');
  if (add) addTag(add.dataset.add);
});
function setTags(tags) {
  editor.replaceAll(withTags(editor.text, tags));
  renderTags([]);
}
function addTag(raw) {
  const t = raw.trim().replace(/^#/, '').replace(/\s+/g, '-').toLowerCase();
  if (!t) return;
  const tags = fmTags(splitFrontmatter(editor.text).yaml);
  if (!tags.map(x => x.toLowerCase()).includes(t)) setTags([...tags, t]);
}
let tagSel = 0;
$('tag-in').addEventListener('input', async () => {
  const q = $('tag-in').value.trim().toLowerCase().replace(/^#/, '');
  const pop = $('tagsug');
  if (!q) { pop.hidden = true; return }
  const all = (tagsCache ||= await api.tags().catch(() => []));
  const m = all.filter(t => t.tag.includes(q)).slice(0, 8);
  tagSel = 0;
  pop.innerHTML = m.map((t, i) => `<button class="${i === 0 ? 'sel' : ''}" data-c="${tagColor(t.tag)}" data-t="${esc(t.tag)}"><i></i>${esc(t.tag)}<small style="margin-left:auto;color:var(--faint)">${t.count}</small></button>`).join('') || `<button data-t="${esc(q)}" class="sel"><i data-c="${tagColor(q)}"></i>Create “${esc(q)}”</button>`;
  place(pop, $('tag-in'));
});
$('tag-in').addEventListener('keydown', e => {
  const items = [...$('tagsug').querySelectorAll('[data-t]')];
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    tagSel = (tagSel + (e.key === 'ArrowDown' ? 1 : items.length - 1)) % Math.max(1, items.length);
    items.forEach((b, i) => b.classList.toggle('sel', i === tagSel));
  } else if (e.key === 'Enter') {
    e.preventDefault();
    const pick = !$('tagsug').hidden && items[tagSel] ? items[tagSel].dataset.t : $('tag-in').value;
    addTag(pick);
    $('tag-in').value = '';
    $('tagsug').hidden = true;
  } else if (e.key === 'Escape') { $('tag-in').value = ''; $('tagsug').hidden = true }
});
$('tagsug').addEventListener('mousedown', e => {
  const b = e.target.closest('[data-t]');
  if (b) { e.preventDefault(); addTag(b.dataset.t); $('tag-in').value = ''; $('tagsug').hidden = true }
});

/* ---------- below the note: backlinks, mentions, related, suggestions ---------- */
let belowSeq = 0;
async function loadBelow(path) {
  const seq = ++belowSeq;
  const [back, mentions, related, sug] = await Promise.all([
    api.backlinks(path).catch(() => []),
    api.mentions(path).catch(() => []),
    api.related(path).catch(() => []),
    api.suggest(path).catch(() => ({ links: [], tags: [], dupes: [] })),
  ]);
  if (seq !== belowSeq || cur?.path !== path) return;
  // group backlinks by source note
  const bySrc = new Map();
  for (const c of back) { if (!bySrc.has(c.path)) bySrc.set(c.path, []); bySrc.get(c.path).push(c) }
  $('links-wrap').hidden = !bySrc.size;
  $('back').innerHTML = [...bySrc.values()].map(cs => `<button class="hit" data-path="${esc(cs[0].path)}" data-pos="${cs[0].start}"><b>${esc(cs[0].title)}<span class="where">${esc(folderOf(cs[0].path))}</span></b><small>${cs.map(c => highlightLink(c.line)).join(' · ')}</small></button>`).join('');
  $('mentions-wrap').hidden = !mentions.length;
  $('mentions').innerHTML = mentions.slice(0, 12).map((c, i) => `<div class="ctx"><button class="hit" data-path="${esc(c.path)}" data-pos="${c.start}"><b>${esc(c.title)}</b><small>${esc(c.line)}</small></button><button class="btn-s" data-link="${i}" title="Turn this mention into a link">Link</button></div>`).join('');
  $('mentions').onclick = async e => {
    const b = e.target.closest('[data-link]');
    if (!b) return;
    const c = mentions[+b.dataset.link];
    try { await api.linkMention(c.path, c.start, c.end, path); toast(`Linked in “${c.title}”`); loadBelow(path) } catch (err) { fail(err) }
  };
  $('rel-wrap').hidden = !related.length;
  $('rel').innerHTML = related.map(r => `<button class="hit" data-path="${esc(r.path)}"><b>${esc(r.title)}<span class="why">${esc(r.why.join(' · '))}</span></b></button>`).join('');
  $('sug-wrap').hidden = !sug.links.length;
  $('sug').innerHTML = sug.links.map((s, i) => `<div class="ctx"><button class="hit" data-path="${esc(s.path)}"><b>${esc(s.title)}<span class="why">${esc(s.reason)}</span></b></button><button class="btn-s" data-ins="${i}" title="Insert a link at the cursor">Insert link</button></div>`).join('');
  $('sug').onclick = e => {
    const b = e.target.closest('[data-ins]');
    if (!b) return;
    editor.insertAtCursor(`[[${sug.links[+b.dataset.ins].title}]]`);
    b.closest('.ctx').remove();
  };
  $('below').hidden = $('links-wrap').hidden && $('mentions-wrap').hidden && $('rel-wrap').hidden && $('sug-wrap').hidden;
  lastSuggested = sug.tags;
  renderTags(sug.tags);
  $('dupe').hidden = !sug.dupes.length;
  if (sug.dupes.length) $('dupe').innerHTML = `Looks almost identical to <button data-path="${esc(sug.dupes[0].path)}">${esc(sug.dupes[0].title)}</button> (${Math.round(sug.dupes[0].jaccard * 100)}% overlap).`;
  cur.backlinks = bySrc.size;
  renderMeta();
}
const highlightLink = line => esc(line).replace(/\[\[([^\]]+)\]\]/g, (_, t) => `<b>${t.split('|').pop()}</b>`);
$('below').addEventListener('click', e => {
  const h = e.target.closest('.hit[data-path]');
  if (h) openNote(h.dataset.path, { newTab: e.metaKey || e.ctrlKey, pos: h.dataset.pos ? +h.dataset.pos : undefined });
});
$('dupe').addEventListener('click', e => { const b = e.target.closest('[data-path]'); if (b) openNote(b.dataset.path) });

/* ---------- note menu ---------- */
$('more').onclick = e => { e.stopPropagation(); const m = $('menu'); m.hidden ? place(m, $('more')) : (m.hidden = true); m.style.left = ''; m.style.right = '12px' };
$('m-copy').onclick = () => { $('menu').hidden = true; navigator.clipboard.writeText(editor.text).then(() => toast('Copied as Markdown'), () => toast('Copying is blocked here.')) };
$('m-path').onclick = () => { $('menu').hidden = true; navigator.clipboard.writeText(cur.path).then(() => toast('Path copied')) };
$('m-dup').onclick = async () => { $('menu').hidden = true; flush(); try { const r = await api.create(folderOf(cur.path), cur.title + ' copy', editor.text); refreshSidebar(); openNote(r.path) } catch (e) { fail(e) } };
$('m-reveal').onclick = () => { $('menu').hidden = true; explore('graph', cur.path) };
$('m-del').onclick = () => { $('menu').hidden = true; trashCurrent() };
$('graph-btn').onclick = () => cur && explore('graph', cur.path);
async function trashCurrent() {
  if (!cur) return;
  flush();
  await saving;
  const p = cur.path;
  try {
    await api.del(p);
    closeTabByPath(p);
    refreshSidebar();
    toast(`Moved “${titleOf(p)}” to the vault's .trash folder`);
  } catch (e) { fail(e) }
}

/* ---------- tabs ---------- */
function renderTabs() {
  const show = ws.tabs.length > 1;
  $('tabs').hidden = !show;
  $('crumb').hidden = view !== 'page' || show;
  if (!show) return;
  $('tabs').innerHTML = ws.tabs.map((p, i) => `<div class="tab${i === ws.active && view === 'page' ? ' on' : ''}" role="tab" data-i="${i}" title="${esc(p)}"><span>${esc(titleOf(p))}</span><button class="x" data-close="${i}" aria-label="Close tab">×</button></div>`).join('');
}
$('tabs').addEventListener('mousedown', e => {
  const x = e.target.closest('[data-close]');
  if (x) { e.preventDefault(); return closeTab(+x.dataset.close) }
  const t = e.target.closest('.tab');
  if (t && e.button === 1) { e.preventDefault(); return closeTab(+t.dataset.i) }
  if (t) { ws.active = +t.dataset.i; openNote(ws.tabs[ws.active]) }
});
function closeTab(i) {
  ws.tabs.splice(i, 1);
  if (!ws.tabs.length) { ws.active = 0; saveWs(); return goHome() }
  if (ws.active >= i) ws.active = Math.max(0, ws.active - 1);
  saveWs();
  openNote(ws.tabs[ws.active]);
}
function closeTabByPath(p) {
  const i = ws.tabs.indexOf(p);
  if (i >= 0) closeTab(i); else if (cur?.path === p) goHome();
}
function retargetTabs(from, to) {
  ws.tabs = ws.tabs.map(p => (p === from ? to : p));
  if (ws.last === from) ws.last = to;
  saveWs();
  renderTabs();
}
const saveWs = debounce(() => api.workspace(ws).catch(() => {}), 300);

/* ---------- new note ---------- */
async function newNote(folder) {
  const f = folder ?? (cur ? folderOf(cur.path) : '');
  try {
    const r = await api.create(f, 'Untitled', '');
    refreshSidebar();
    await openNote(r.path, { newTab: false });
    $('title').focus();
    $('title').select();
  } catch (e) { fail(e) }
}
$('new').onclick = () => newNote();

/* ---------- palette: one box to find notes, search text, and run commands ---------- */
const COMMANDS = [
  ['New note', '⌘N', () => newNote()],
  ['New folder', '', () => { setSideTab('notes'); $('f-add').click() }],
  ['Go home', '', goHome],
  ['Search everything', '⌘⇧F', () => explore('search')],
  ['Graph of this note', '⌘G', () => explore('graph', cur?.path)],
  ['Graph of all notes', '', () => explore('graph')],
  ['Browse tags', '', () => explore('tags')],
  ['Browse topics', '', () => explore('topics')],
  ['Vault data', '', () => explore('data')],
  ['Toggle theme', '', () => $('theme-btn').click()],
  ['Open settings', '⌘,', () => openSettings()],
  ['Toggle sidebar', '⌘\\', () => setSide(app.classList.contains('collapsed'))],
  ['Rename note', '', () => { if (cur) { $('title').focus(); $('title').select() } }],
  ['Move note to trash', '', trashCurrent],
  ['Copy note as Markdown', '', () => $('m-copy').click()],
  ['Find in note', '⌘F', () => { if (cur) { editor.focus(); import('@codemirror/search').then(m => m.openSearchPanel(editor.view)) } }],
  ['Rescan vault for changes', '', () => api.rescan().then(() => toast('Rescanning the vault'))],
  ['Close tab', '⌘W', () => closeTab(ws.active)],
];
let palRows = [];
let palSel = 0;
let palMode = 'notes';
function openFind(prefix = '') {
  $('veil').hidden = false;
  $('q').value = prefix;
  palMode = prefix.startsWith('>') ? 'commands' : 'notes';
  runPal();
  $('q').focus();
}
function closePal() { $('veil').hidden = true }
$('find').onclick = () => openFind();
$('veil').addEventListener('mousedown', e => { if (e.target === $('veil')) closePal() });
let palSeq = 0;
async function runPal() {
  const raw = $('q').value;
  const seq = ++palSeq;
  if (raw.startsWith('>')) {
    const q = raw.slice(1).trim().toLowerCase();
    palRows = COMMANDS.filter(c => c[0].toLowerCase().includes(q)).map(c => ({ kind: 'cmd', label: c[0], key: c[1], run: c[2] }));
    return drawPal();
  }
  const q = raw.trim();
  const t0 = performance.now();
  const sw = await api.switch(q, q ? 12 : 10).catch(() => []);
  if (seq !== palSeq) return;
  palRows = sw.map(s => ({ kind: 'note', path: s.path, label: s.title, sub: s.alias ? `alias: ${s.alias}` : folderOf(s.path), typo: s.typo }));
  if (q) palRows.push({ kind: 'create', label: `Create “${q}”`, q });
  $('stat').textContent = `${(performance.now() - t0).toFixed(0)} ms`;
  drawPal(q ? '' : 'Recent');
  if (q.length >= 2) palSearch(q, seq);
}
const palSearch = debounce(async (q, seq) => {
  const r = await api.search(q, 'hybrid', 8).catch(() => null);
  if (!r || seq !== palSeq) return;
  const have = new Set(palRows.filter(x => x.kind === 'note').map(x => x.path));
  const extra = r.results.filter(x => !have.has(x.path)).map(x => ({ kind: 'text', path: x.path, label: x.title, sub: x.section || folderOf(x.path), snippet: x.snippet }));
  const create = palRows.findIndex(x => x.kind === 'create');
  palRows.splice(create < 0 ? palRows.length : create, 0, ...extra);
  $('stat').textContent = `${r.results.length} text matches · ${r.ms.toFixed(1)} ms`;
  drawPal();
}, 110);
function drawPal(heading) {
  palSel = Math.min(palSel, Math.max(0, palRows.length - 1));
  if (palRows.length && palSel < 0) palSel = 0;
  let lastKind = null;
  $('res').innerHTML = palRows.map((r, i) => {
    let head = '';
    if (r.kind !== lastKind) {
      const label = { note: heading || 'Notes', text: 'In text', cmd: 'Commands', create: '' }[r.kind];
      if (label) head = `<div class="group">${label}</div>`;
      lastKind = r.kind;
    }
    const icon = { note: ICON.note, text: ICON.search, cmd: ICON.command, create: ICON.plus }[r.kind];
    const right = r.kind === 'cmd' ? (r.key ? `<span class="kbd">${esc(r.key)}</span>` : '') : `<small>${esc(r.sub || '')}${r.typo ? ' · close match' : ''}</small>`;
    return `${head}<button class="cmd${i === palSel ? ' sel' : ''}" data-i="${i}" role="option">${SVG(icon)}<span>${esc(r.label)}${r.snippet ? `<span class="snip">${r.snippet}</span>` : ''}</span>${right}</button>`;
  }).join('') || `<div class="empty">Nothing yet. Press ↵ to create it.</div>`;
  $('res').querySelector('.sel')?.scrollIntoView({ block: 'nearest' });
}
function runRow(r, newTab) {
  if (!r) return;
  closePal();
  if (r.kind === 'cmd') return r.run();
  if (r.kind === 'create') return api.create(cur ? folderOf(cur.path) : '', r.q, '').then(x => { refreshSidebar(); openNote(x.path, { newTab, focusEnd: true }) }).catch(fail);
  openNote(r.path, { newTab });
}
$('q').addEventListener('input', () => { palSel = 0; runPal() });
$('q').addEventListener('keydown', e => {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    palSel = (palSel + (e.key === 'ArrowDown' ? 1 : palRows.length - 1)) % Math.max(1, palRows.length);
    drawPal();
  } else if (e.key === 'Enter') { e.preventDefault(); runRow(palRows[palSel], e.metaKey || e.ctrlKey) }
  else if (e.key === 'Escape') closePal();
});
$('res').addEventListener('click', e => { const b = e.target.closest('[data-i]'); if (b) runRow(palRows[+b.dataset.i], e.metaKey || e.ctrlKey) });

/* ---------- explore views ---------- */
let graphHandle = null;
async function explore(kind, arg) {
  flush();
  exView = { kind, arg };
  cur = kind === 'graph' && arg ? cur : null;
  show('explore');
  graphHandle?.destroy();
  graphHandle = null;
  const ex = $('explore');
  ex.classList.toggle('wide', kind === 'graph');
  ex.innerHTML = '';
  markActiveInSidebar();
  try {
    if (kind === 'search') return exploreSearch(ex, arg);
    if (kind === 'tags') return exploreTags(ex, arg);
    if (kind === 'topics') return exploreTopics(ex);
    if (kind === 'graph') return exploreGraph(ex, arg);
    if (kind === 'data') return exploreData(ex);
  } catch (e) { fail(e) }
}

function exploreSearch(ex, q0 = '') {
  let mode = store.get('cera.searchMode', 'hybrid');
  ex.innerHTML = `<h1 class="ex-title">Search</h1>
    <div class="search-box">${SVG(ICON.search)}<input id="xs" placeholder="Words, &quot;phrases&quot;, tag:, path:, title:, -exclude" spellcheck="false"></div>
    <div class="search-meta"><span class="seg"><button data-m="hybrid">Meaning + words</button><button data-m="text">Exact words</button></span><span id="xs-stat"></span></div>
    <div id="xs-res"></div>
    <p class="syntax">Try <code>"zero copy"</code> <code>tag:project</code> <code>path:Journal</code> <code>title:plan</code> <code>-draft</code>. Typos are forgiven. “Meaning + words” blends keyword ranking with local embeddings.</p>`;
  const inp = ex.querySelector('#xs');
  const seg = () => ex.querySelectorAll('[data-m]').forEach(b => b.classList.toggle('on', b.dataset.m === mode));
  seg();
  let seq = 0;
  const run = debounce(async () => {
    const q = inp.value.trim();
    const s = ++seq;
    if (!q) { ex.querySelector('#xs-res').innerHTML = ''; ex.querySelector('#xs-stat').textContent = ''; return }
    const r = await api.search(q, mode, 50).catch(fail);
    if (!r || s !== seq) return;
    ex.querySelector('#xs-stat').textContent = `${r.results.length} notes · ${r.ms.toFixed(1)} ms`;
    ex.querySelector('#xs-res').innerHTML = r.results.map(x => `<button class="hit" data-path="${esc(x.path)}"><b>${esc(x.title)}<span class="where">${esc(x.section || folderOf(x.path))}</span></b><small>${x.snippet}</small></button>`).join('') || `<div class="empty">No notes match.</div>`;
  }, 120);
  inp.oninput = run;
  ex.querySelector('.seg').onclick = e => { const b = e.target.closest('[data-m]'); if (b) { mode = b.dataset.m; store.set('cera.searchMode', mode); seg(); run() } };
  ex.querySelector('#xs-res').onclick = e => { const h = e.target.closest('[data-path]'); if (h) openNote(h.dataset.path, { newTab: e.metaKey || e.ctrlKey }) };
  inp.value = q0;
  inp.focus();
  if (q0) run();
}

async function exploreTags(ex, tag) {
  if (tag) {
    const notes = await api.tag(tag);
    ex.innerHTML = `<h1 class="ex-title"><span class="tag-chip" data-c="${tagColor(tag)}" style="font-size:22px;height:36px;padding:0 16px">${esc(tag)}</span></h1>
      <p class="ex-sub">${fmt(notes.length)} ${notes.length === 1 ? 'note' : 'notes'}</p><div id="xt"></div>`;
    ex.querySelector('#xt').append(noteList(notes.map(n => ({ path: n.path, title: n.title, markdown: true }))));
    ex.querySelector('#xt').onclick = e => { const b = e.target.closest('[data-path]'); if (b) openNote(b.dataset.path, { newTab: e.metaKey || e.ctrlKey }) };
    return;
  }
  const tags = (tagsCache = await api.tags());
  const max = Math.max(1, ...tags.map(t => t.count));
  ex.innerHTML = `<h1 class="ex-title">Tags</h1><p class="ex-sub">${fmt(tags.length)} tags across your notes. Nested tags roll up into their parents.</p>
    <div>${tags.slice(0, 300).map(t => `<button class="ex-tagrow" data-c="${tagColor(t.tag)}" data-tag="${esc(t.tag)}"><span class="ex-name"><span class="ex-dot"></span>${esc(t.tag)}</span><span class="count">${fmt(t.count)}</span><span class="ex-bar"><i style="width:${(t.count / max) * 100}%"></i></span></button>`).join('')}</div>`;
  ex.onclick = e => { const b = e.target.closest('[data-tag]'); if (b) explore('tags', b.dataset.tag) };
}

async function exploreTopics(ex) {
  const cs = await api.clusters();
  ex.innerHTML = `<h1 class="ex-title">Topics</h1><p class="ex-sub">Groups of notes that link to each other more than to anything else, found with Leiden community detection. Each card shows its most central notes.</p>
    ${cs.slice(0, 60).map((c, i) => `<section class="ex-card" data-c="${i % 8}"><header><span class="ex-dot"></span><b>${esc(c.label)}</b><span class="where" style="color:var(--faint);font-size:12px">${fmt(c.size)} notes</span>${c.tags.length ? `<span class="ex-chips" style="margin-left:auto">${c.tags.map(t => `<span class="tag-chip" data-c="${tagColor(t)}" style="padding:0 9px">${esc(t)}</span>`).join('')}</span>` : ''}</header>
      ${c.top.slice(0, 5).map(n => `<button class="hit" data-path="${esc(n.path)}"><b>${esc(n.title)}<span class="where">${esc(folderOf(n.path))}</span></b></button>`).join('')}</section>`).join('') || '<div class="empty">Topics appear once notes link to each other.</div>'}`;
  ex.onclick = e => { const b = e.target.closest('[data-path]'); if (b) openNote(b.dataset.path, { newTab: e.metaKey || e.ctrlKey }) };
}

// Two views of one graph: "This note" (the open note and its neighbours, 1 or 2 links away) and
// "All notes" (the whole vault). Switching reloads only the canvas; the toolbar keeps its size.
async function exploreGraph(ex, path) {
  const note = path || cur?.path || null;
  let local = !!path;
  let depth = 2;
  ex.innerHTML = `<div id="graph-wrap"><canvas id="graph"></canvas>
    <div class="graph-bar"><span class="ttl"></span>
      <span class="seg" role="tablist" aria-label="Graph scope">${note ? `<button data-g="local" role="tab">This note</button>` : ''}<button data-g="global" role="tab">All notes</button></span>
      <span class="seg" id="g-depth" aria-label="How far to look"><button data-d="1">1 link away</button><button data-d="2">2 links away</button></span>
    </div><div class="graph-legend" id="g-legend">Loading…</div></div>`;
  const bar = ex.querySelector('.graph-bar');
  const paint = () => {
    bar.querySelector('.ttl').textContent = local ? titleOf(note) : vaultName;
    bar.querySelectorAll('[data-g]').forEach(b => { const on = (b.dataset.g === 'local') === local; b.classList.toggle('on', on); b.setAttribute('aria-selected', on) });
    bar.querySelectorAll('[data-d]').forEach(b => b.classList.toggle('on', +b.dataset.d === depth));
    $('g-depth').style.visibility = local ? '' : 'hidden';
  };
  let seq = 0;
  const load = async () => {
    const s = ++seq;
    paint();
    $('g-legend').textContent = 'Loading…';
    const t0 = performance.now();
    const data = await api.graph(local ? note : undefined, depth, 20000);
    if (s !== seq || exView?.kind !== 'graph') return;
    graphHandle?.destroy();
    graphHandle = mountGraph($('graph'), data, { onOpen: (p, newTab) => openNote(p, { newTab }), tip: ex.querySelector('#graph-wrap') });
    $('g-legend').textContent = data.aggregated
      ? `${fmt(data.nodes.length)} topics shown as single nodes (zoomed out level of detail) · scroll to zoom, drag to pan`
      : `${fmt(data.nodes.length)} notes · ${fmt(data.edges.length)} links · ${(performance.now() - t0).toFixed(0)} ms · scroll to zoom, drag to pan, click to open`;
  };
  bar.onclick = e => {
    const g = e.target.closest('[data-g]');
    if (g && (g.dataset.g === 'local') !== local) { local = g.dataset.g === 'local'; exView.arg = local ? note : undefined; return load() }
    const d = e.target.closest('[data-d]');
    if (d && +d.dataset.d !== depth) { depth = +d.dataset.d; load() }
  };
  await load();
}

async function exploreData(ex) {
  const [s, st] = await Promise.all([api.stats(), api.status()]);
  const max = Math.max(1, ...s.activity);
  const level = v => (v === 0 ? 0 : v / max > 0.66 ? 3 : v / max > 0.33 ? 2 : 1);
  const t = st.timings;
  ex.innerHTML = `<h1 class="ex-title">Your vault</h1><p class="ex-sub">Everything below is computed locally from your Markdown files.</p>
    <div class="ex-tiles">
      <div class="ex-tile"><b>${fmt(s.notes)}</b><span>notes</span></div>
      <div class="ex-tile"><b>${fmt(s.words)}</b><span>words</span></div>
      <div class="ex-tile"><b>${fmt(s.links)}</b><span>links</span></div>
      <div class="ex-tile"><b>${fmt(s.tags)}</b><span>tags</span></div>
      <div class="ex-tile"><b>${fmt(s.folders)}</b><span>folders</span></div>
      <div class="ex-tile"><b>${fmt(s.attachments)}</b><span>attachments</span></div>
      <div class="ex-tile"><b>${fmt(s.orphans)}</b><span>unlinked notes</span></div>
      <div class="ex-tile"><b>${fmt(s.tasks_open)}</b><span>open tasks</span></div>
    </div>
    <h2>Writing activity, last 26 weeks</h2>
    <div class="heat">${s.activity.map(v => `<i data-l="${level(v)}" title="${v} notes"></i>`).join('')}</div>
    <p class="heat-note">${fmt(s.activity.reduce((a, b) => a + b, 0))} notes changed in this period.</p>
    <h2>Links to notes that don't exist yet</h2><p class="ex-sub" style="margin:0">${fmt(s.unresolved)} distinct targets. Click one in a note to create it.</p>
    <h2>Near-duplicates</h2><div id="xd"><div class="empty">Checking…</div></div>
    <h2>Engine</h2>
    <p class="syntax">Opened in ${t.open_total_ms.toFixed(1)} ms (snapshot ${t.snapshot_map_ms.toFixed(1)} ms) · file table ready ${t.hydrate_core_ms.toFixed(0)} ms · links ${t.hydrate_ms.toFixed(0)} ms · ${st.vectors ? `${fmt(st.vectors)} passages embedded with <code>${esc(st.model)}</code>` : 'embeddings pending'} · ${st.embedding_pending ? fmt(st.embedding_pending) + ' notes queued' : 'up to date'}</p>`;
  const d = await api.dupes().catch(() => []);
  ex.querySelector('#xd').innerHTML = d.length
    ? d.slice(0, 20).map(([a, b, j]) => `<div class="ctx"><button class="hit" data-path="${esc(a.path)}"><b>${esc(a.title)}<span class="why">${Math.round(j * 100)}% same as ${esc(b.title)}</span></b></button><button class="btn-s" data-path="${esc(b.path)}" style="opacity:1">Open other</button></div>`).join('')
    : '<div class="empty">No near-duplicate notes.</div>';
  ex.onclick = e => { const b = e.target.closest('[data-path]'); if (b) openNote(b.dataset.path, { newTab: e.metaKey || e.ctrlKey }) };
}

/* ---------- settings: a small sheet from the bottom-left of the sidebar ---------- */
const SETTINGS_DEFAULT = { size: 'default', width: 'default', spellcheck: false, counts: true };
const settings = Object.assign({}, SETTINGS_DEFAULT, store.get('cera.settings', {}));
const SIZES = { small: '15px', default: '16px', large: '18px' };
const WIDTHS = { narrow: '620px', default: '720px', wide: '880px' };
function applySettings() {
  const r = document.documentElement.style;
  r.setProperty('--ed-size', SIZES[settings.size] || SIZES.default);
  r.setProperty('--page-w', WIDTHS[settings.width] || WIDTHS.default);
  editor.view.contentDOM.spellcheck = !!settings.spellcheck;
  document.body.classList.toggle('no-counts', !settings.counts);
  editor.view.requestMeasure();
}
function setSetting(k, v) {
  settings[k] = v;
  store.set('cera.settings', settings);
  applySettings();
  renderSettings();
}
const seg = (key, value, options) => `<span class="seg" role="radiogroup">${options.map(([v, label]) => `<button role="radio" aria-checked="${v === value}" class="${v === value ? 'on' : ''}" data-set="${key}" data-v="${v}">${label}</button>`).join('')}</span>`;
const toggle = (key, on, label) => `<button class="switch${on ? ' on' : ''}" role="switch" aria-checked="${on}" aria-label="${label}" data-toggle="${key}"><i></i></button>`;
const srow = (label, hint, control) => `<div class="s-row"><div><b>${label}</b>${hint ? `<small>${hint}</small>` : ''}</div>${control}</div>`;
function renderSettings() {
  const st = status || {};
  const keys = [['⌘K', 'Find a note'], ['⌘N', 'New note'], ['⌘P', 'Commands'], ['⌘G', 'Graph of this note'], ['⌘\\', 'Toggle sidebar'], ['⌘,', 'Settings']];
  $('settings-body').innerHTML = `
    <section><h3>Appearance</h3>
      ${srow('Theme', '', seg('theme', theme, [['system', 'System'], ['light', 'Light'], ['dark', 'Dark']]))}
    </section>
    <section><h3>Editor</h3>
      ${srow('Text size', '', seg('size', settings.size, [['small', 'Small'], ['default', 'Default'], ['large', 'Large']]))}
      ${srow('Line width', '', seg('width', settings.width, [['narrow', 'Narrow'], ['default', 'Default'], ['wide', 'Wide']]))}
      ${srow('Spell check', 'Underline misspelled words while you write', toggle('spellcheck', settings.spellcheck, 'Spell check'))}
    </section>
    <section><h3>Sidebar</h3>
      ${srow('Note counts', 'Show how many notes each folder holds', toggle('counts', settings.counts, 'Note counts'))}
    </section>
    <section><h3>Vault</h3>
      ${srow(esc(vaultName), `${fmt(st.notes || 0)} notes · ${st.ready ? (st.embedding_pending ? `embedding ${fmt(st.embedding_pending)}` : 'up to date') : 'indexing…'}`, `<button class="btn quiet" id="s-rescan">Rescan</button>`)}
      <p class="s-note">Your notes are plain Markdown files in this folder. Search data lives in <code>.vault/</code> and can be rebuilt at any time.</p>
    </section>
    <section><h3>Keyboard</h3>
      <div class="s-keys">${keys.map(([k, l]) => `<span>${l}</span><span class="kbd">${esc(k)}</span>`).join('')}</div>
    </section>`;
}
function openSettings() {
  renderSettings();
  $('settings-veil').hidden = false;
  $('settings-close').focus();
}
function closeSettings() { $('settings-veil').hidden = true; $('settings-btn').focus() }
$('settings-btn').onclick = openSettings;
$('settings-close').onclick = closeSettings;
$('settings-veil').addEventListener('mousedown', e => { if (e.target === $('settings-veil')) closeSettings() });
$('settings-body').addEventListener('click', e => {
  const b = e.target.closest('[data-set]');
  if (b) return b.dataset.set === 'theme' ? setTheme(b.dataset.v) : setSetting(b.dataset.set, b.dataset.v);
  const t = e.target.closest('[data-toggle]');
  if (t) return setSetting(t.dataset.toggle, !settings[t.dataset.toggle]);
  if (e.target.closest('#s-rescan')) api.rescan().then(() => toast('Rescanning the vault'), fail);
});
applySettings();

/* ---------- keyboard ---------- */
addEventListener('keydown', e => {
  const mod = e.metaKey || e.ctrlKey;
  const k = e.key.toLowerCase();
  if (mod && (k === 'k' || k === 'o') && !e.shiftKey) { e.preventDefault(); $('veil').hidden ? openFind() : closePal() }
  else if (mod && k === 'p') { e.preventDefault(); openFind('>') }
  else if (mod && e.shiftKey && k === 'f') { e.preventDefault(); explore('search') }
  else if (mod && k === 'n') { e.preventDefault(); newNote() }
  else if (e.altKey && e.code === 'KeyN') { e.preventDefault(); newNote() }
  else if (mod && k === 'g') { e.preventDefault(); explore('graph', cur?.path) }
  else if (mod && k === 'w' && view === 'page' && ws.tabs.length) { e.preventDefault(); closeTab(ws.active) }
  else if (mod && k === 's') { e.preventDefault(); save() }
  else if (mod && e.key === ',') { e.preventDefault(); $('settings-veil').hidden ? openSettings() : closeSettings() }
  else if (mod && e.key === '\\') { e.preventDefault(); setSide(app.classList.contains('collapsed')) }
  else if (e.ctrlKey && e.key === 'Tab' && ws.tabs.length > 1) { e.preventDefault(); ws.active = (ws.active + (e.shiftKey ? ws.tabs.length - 1 : 1)) % ws.tabs.length; openNote(ws.tabs[ws.active]) }
  else if (e.key === 'Escape') { if (!$('settings-veil').hidden) closeSettings(); else if (!$('veil').hidden) closePal(); else ['menu', 'move', 'fmenu', 'tagsug'].forEach(id => ($(id).hidden = true)) }
});
document.addEventListener('mousedown', e => {
  if (!e.target.closest('#menu, #more')) $('menu').hidden = true;
  if (!e.target.closest('#move, #crumb')) $('move').hidden = true;
  if (!e.target.closest('#fmenu, [data-act="more"]')) $('fmenu').hidden = true;
  if (!e.target.closest('#tagsug, #tag-in')) $('tagsug').hidden = true;
});
addEventListener('resize', () => { if (mobile()) setSide(false); else $('scrim').hidden = true });
addEventListener('focus', () => api.rescan().catch(() => {})); // FSEvents can coalesce; verify on focus
addEventListener('beforeunload', () => { if (dirty) navigator.sendBeacon?.('/api/note', '') });
addEventListener('pagehide', () => flush());

/* ---------- live updates from the engine ---------- */
function connectEvents() {
  const es = new EventSource('/api/events');
  es.addEventListener('change', async ev => {
    const c = JSON.parse(ev.data);
    refreshSidebar();
    tagsCache = null;
    if (cur && c.paths.includes(cur.path)) {
      if (c.external && !dirty) {
        const n = await api.note(cur.path).catch(() => null);
        if (n && n.hash !== cur.hash) {
          cur.hash = n.hash;
          editor.set(n.content, { keepSelection: true });
          toast('Updated from disk');
        } else if (!n) {
          toast('This note was moved or deleted outside Cera.');
        }
      }
    }
  });
  es.onerror = () => { es.close(); setTimeout(connectEvents, 1500) };
}

async function pollStatus() {
  try {
    status = await api.status();
    const s = status;
    $('index-state').textContent = !s.ready
      ? `Indexing… ${fmt(s.notes)} notes`
      : s.embedding_pending
        ? `${fmt(s.notes)} notes · embedding ${fmt(s.embedding_pending)}`
        : `${fmt(s.notes)} notes`;
    if (sideTab === 'explore') renderExNav();
    setTimeout(pollStatus, s.ready && !s.embedding_pending ? 10_000 : 800);
  } catch { setTimeout(pollStatus, 1500) }
}

/* ---------- start ---------- */
(async function start() {
  setSide(!mobile() && store.get('cera.side', true));
  setSideTab(sideTab);
  let boot;
  try { boot = await api.boot() } catch (e) {
    $('home').hidden = false;
    $('matches').innerHTML = `<p class="hint">Couldn't reach the Vault engine. ${esc(e.message)}</p>`;
    return;
  }
  ws = Object.assign({ tabs: [], active: 0, last: null }, boot.workspace || {});
  ws.tabs = (ws.tabs || []).filter(Boolean);
  if (boot.vault) { vaultName = boot.vault; document.title = `${boot.vault} — Cera` }
  // First frame: the last note straight from disk, before the index is ready.
  if (boot.first) {
    if (!ws.tabs.includes(boot.first.path)) { ws.tabs = [boot.first.path]; ws.active = 0 }
    await openNote(boot.first.path);
  } else goHome();
  connectEvents();
  pollStatus();
  renderSidebar(false);
})();
