// Thin client for the vault-server JSON API. All engine work happens server-side, off the UI thread.
async function req(method, url, body) {
  const r = await fetch(url, {
    method,
    headers: body !== undefined ? { 'content-type': 'application/json' } : {},
    body: body !== undefined ? JSON.stringify(body) : undefined,
    credentials: 'same-origin',
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) { const e = new Error(data.message || r.statusText); e.status = r.status; e.kind = data.error; throw e }
  return data;
}
const q = o => new URLSearchParams(Object.entries(o).filter(([, v]) => v !== undefined && v !== null)).toString();
export const api = {
  boot: () => req('GET', '/api/boot'),
  vaults: () => req('GET', '/api/vaults'),
  openVault: path => req('POST', '/api/vaults/open', { path }),
  forgetVault: path => req('POST', '/api/vaults/forget', { path }),
  status: () => req('GET', '/api/status'),
  dir: path => req('GET', `/api/dir?${q({ path })}`),
  recent: limit => req('GET', `/api/recent?${q({ limit })}`),
  note: path => req('GET', `/api/note?${q({ path })}`),
  save: (path, content, base) => req('PUT', '/api/note', { path, content, base }),
  create: (folder, title, content) => req('POST', '/api/note', { folder, title, content }),
  rename: (from, to) => req('POST', '/api/rename', { from, to }),
  del: path => req('POST', '/api/delete', { path }),
  mkdir: path => req('POST', '/api/folder', { path }),
  renameFolder: (from, to) => req('POST', '/api/folder/rename', { from, to }),
  delFolder: path => req('POST', '/api/folder/delete', { path }),
  switch: (qq, limit = 20) => req('GET', `/api/switch?${q({ q: qq, limit })}`),
  search: (qq, mode = 'hybrid', limit = 30) => req('GET', `/api/search?${q({ q: qq, mode, limit })}`),
  resolve: (target, from) => req('GET', `/api/resolve?${q({ target, from })}`),
  backlinks: path => req('GET', `/api/backlinks?${q({ path })}`),
  mentions: path => req('GET', `/api/mentions?${q({ path })}`),
  linkMention: (src, start, end, target) => req('POST', '/api/link-mention', { src, start, end, target }),
  related: path => req('GET', `/api/related?${q({ path })}`),
  suggest: path => req('GET', `/api/suggest?${q({ path })}`),
  tags: () => req('GET', '/api/tags'),
  tag: tag => req('GET', `/api/tag?${q({ tag })}`),
  clusters: () => req('GET', '/api/clusters'),
  graph: (path, depth, max) => req('GET', `/api/graph?${q({ path, depth, max })}`),
  stats: () => req('GET', '/api/stats'),
  dupes: () => req('GET', '/api/dupes'),
  workspace: ws => req('PUT', '/api/workspace', ws),
  rescan: () => req('POST', '/api/rescan'),
  recall: (qq, limit) => req('GET', `/api/recall?${q({ q: qq, limit })}`),
  ai: () => req('GET', '/api/ai'),
  setAi: patch => req('PUT', '/api/ai', patch),
  aiModels: url => req('GET', `/api/ai/models?${q({ url })}`),
  // Streams an answer: calls on(event, data) for sources, model, delta, done and error.
  async ask(body, on, signal) {
    const r = await fetch('/api/ask', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body), credentials: 'same-origin', signal });
    if (!r.ok) throw new Error((await r.json().catch(() => ({}))).message || r.statusText);
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    let buf = '';
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true }).replace(/\r\n/g, '\n');
      let i;
      while ((i = buf.indexOf('\n\n')) >= 0) {
        const block = buf.slice(0, i);
        buf = buf.slice(i + 2);
        let ev = 'message', data = '';
        for (const l of block.split('\n')) {
          if (l.startsWith('event:')) ev = l.slice(6).trim();
          else if (l.startsWith('data:')) data += l.slice(5).trimStart();
        }
        if (data) on(ev, JSON.parse(data));
      }
    }
  },
  async upload(note, file) {
    const r = await fetch(`/api/attachment?${q({ note, name: file.name || 'pasted.png' })}`, { method: 'POST', body: file, credentials: 'same-origin' });
    if (!r.ok) throw new Error('upload failed');
    return (await r.json()).path;
  },
};
