// Graph view: positions come from the engine's force layout (persisted in .vault/layout.bin), so
// the picture opens settled. Canvas 2D with batched paths: one stroke for all edges, one fill per
// cluster colour. Labels appear for nodes whose rank clears a zoom-dependent threshold.
export function mountGraph(canvas, data, { onOpen, tip }) {
  const ctx = canvas.getContext('2d');
  const css = getComputedStyle(document.documentElement);
  const colors = Array.from({ length: 8 }, (_, i) => css.getPropertyValue(`--t${i}`).trim() || '#888');
  const fg = css.getPropertyValue('--fg').trim();
  const faint = css.getPropertyValue('--faint').trim();
  const border = css.getPropertyValue('--border').trim();
  const honey = css.getPropertyValue('--honey').trim();
  const bg = css.getPropertyValue('--bg').trim();
  const nodes = data.nodes;
  const index = new Map(nodes.map((n, i) => [n.id, i]));
  const edges = data.edges.map(([a, b]) => [index.get(a), index.get(b)]).filter(([a, b]) => a !== undefined && b !== undefined);
  const deg = new Float32Array(nodes.length);
  for (const [a, b] of edges) { deg[a]++; deg[b]++ }
  const radius = nodes.map((n, i) => (data.aggregated ? 4 + 14 * Math.sqrt(n.rank) : 2.2 + Math.min(7, Math.sqrt(deg[i]) * 0.9 + n.rank * 5)));
  const centerIdx = data.center != null ? index.get(data.center) : -1;
  const neighbors = new Set();
  if (centerIdx >= 0) for (const [a, b] of edges) { if (a === centerIdx) neighbors.add(b); if (b === centerIdx) neighbors.add(a) }

  // fit
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const n of nodes) { minX = Math.min(minX, n.x); maxX = Math.max(maxX, n.x); minY = Math.min(minY, n.y); maxY = Math.max(maxY, n.y) }
  if (!nodes.length) { minX = minY = -1; maxX = maxY = 1 }
  let W = 0, H = 0, dpr = 1;
  let scale = 1, tx = 0, ty = 0;
  const resize = () => {
    dpr = window.devicePixelRatio || 1;
    W = canvas.clientWidth; H = canvas.clientHeight;
    canvas.width = Math.max(1, W * dpr); canvas.height = Math.max(1, H * dpr);
  };
  const fit = () => {
    const w = Math.max(1, maxX - minX), h = Math.max(1, maxY - minY);
    scale = Math.min(W / (w * 1.15), H / (h * 1.15), 4);
    tx = W / 2 - ((minX + maxX) / 2) * scale;
    ty = H / 2 - ((minY + maxY) / 2) * scale;
  };
  resize(); fit();

  // spatial grid for hover hit-testing
  const cell = 24;
  let grid = new Map();
  const rebuildGrid = () => {
    grid = new Map();
    nodes.forEach((n, i) => {
      const k = `${Math.floor(n.x / cell)},${Math.floor(n.y / cell)}`;
      if (!grid.has(k)) grid.set(k, []);
      grid.get(k).push(i);
    });
  };
  rebuildGrid();
  const pick = (sx, sy) => {
    const x = (sx - tx) / scale, y = (sy - ty) / scale;
    const gx = Math.floor(x / cell), gy = Math.floor(y / cell);
    let best = -1, bd = Infinity;
    for (let dx = -1; dx <= 1; dx++) for (let dy = -1; dy <= 1; dy++) {
      for (const i of grid.get(`${gx + dx},${gy + dy}`) || []) {
        const d = (nodes[i].x - x) ** 2 + (nodes[i].y - y) ** 2;
        const r = Math.max(radius[i], 6 / scale);
        if (d < r * r && d < bd) { bd = d; best = i }
      }
    }
    return best;
  };

  let hover = -1;
  let raf = 0;
  const draw = () => {
    raf = 0;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, W, H);
    ctx.setTransform(dpr * scale, 0, 0, dpr * scale, dpr * tx, dpr * ty);
    // edges
    ctx.lineWidth = Math.max(0.4, 1 / scale);
    ctx.strokeStyle = border;
    ctx.globalAlpha = edges.length > 50000 ? 0.35 : 0.8;
    ctx.beginPath();
    for (const [a, b] of edges) { ctx.moveTo(nodes[a].x, nodes[a].y); ctx.lineTo(nodes[b].x, nodes[b].y) }
    ctx.stroke();
    if (hover >= 0 || centerIdx >= 0) {
      const focus = hover >= 0 ? hover : centerIdx;
      ctx.strokeStyle = honey;
      ctx.globalAlpha = 0.9;
      ctx.lineWidth = Math.max(0.8, 1.6 / scale);
      ctx.beginPath();
      for (const [a, b] of edges) if (a === focus || b === focus) { ctx.moveTo(nodes[a].x, nodes[a].y); ctx.lineTo(nodes[b].x, nodes[b].y) }
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    // nodes, batched by colour
    const left = -tx / scale, top = -ty / scale, right = (W - tx) / scale, bottom = (H - ty) / scale;
    for (let c = 0; c < 8; c++) {
      ctx.fillStyle = colors[c];
      ctx.beginPath();
      nodes.forEach((n, i) => {
        if (n.cluster % 8 !== c || n.x < left - 20 || n.x > right + 20 || n.y < top - 20 || n.y > bottom + 20) return;
        ctx.moveTo(n.x + radius[i], n.y);
        ctx.arc(n.x, n.y, radius[i], 0, Math.PI * 2);
      });
      ctx.fill();
    }
    if (centerIdx >= 0) {
      const n = nodes[centerIdx];
      ctx.lineWidth = 2 / scale;
      ctx.strokeStyle = honey;
      ctx.beginPath(); ctx.arc(n.x, n.y, radius[centerIdx] + 3 / scale, 0, Math.PI * 2); ctx.stroke();
    }
    // labels: zoom-dependent rank threshold
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.font = '500 12px Inter, system-ui, sans-serif';
    ctx.textAlign = 'center';
    const threshold = data.aggregated ? 0 : Math.max(0, 0.9 - scale * 0.35);
    let drawn = 0;
    const order = nodes.map((_, i) => i).sort((a, b) => nodes[b].rank - nodes[a].rank);
    for (const i of order) {
      if (drawn > 250) break;
      const n = nodes[i];
      const important = i === hover || i === centerIdx || neighbors.has(i) && scale > 0.6;
      if (!important && n.rank < threshold) continue;
      const sx = n.x * scale + tx, sy = n.y * scale + ty;
      if (sx < -50 || sx > W + 50 || sy < -20 || sy > H + 20) continue;
      ctx.fillStyle = i === hover ? fg : faint;
      ctx.fillText(n.title.length > 40 ? n.title.slice(0, 38) + '…' : n.title, sx, sy - radius[i] * scale - 5);
      drawn++;
    }
  };
  const redraw = () => { if (!raf) raf = requestAnimationFrame(draw) };
  draw();

  // interaction: wheel zoom about the cursor, drag to pan, click to open
  let drag = null;
  const onWheel = e => {
    e.preventDefault();
    const r = canvas.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    const k = Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0015));
    const ns = Math.min(40, Math.max(0.02, scale * k));
    tx = mx - ((mx - tx) * ns) / scale;
    ty = my - ((my - ty) * ns) / scale;
    scale = ns;
    redraw();
  };
  const onDown = e => { drag = { x: e.clientX, y: e.clientY, tx, ty, moved: false }; canvas.classList.add('drag'); canvas.setPointerCapture(e.pointerId) };
  const onMove = e => {
    const r = canvas.getBoundingClientRect();
    if (drag) {
      const dx = e.clientX - drag.x, dy = e.clientY - drag.y;
      if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
      tx = drag.tx + dx; ty = drag.ty + dy;
      redraw();
      return;
    }
    const h = pick(e.clientX - r.left, e.clientY - r.top);
    if (h !== hover) {
      hover = h;
      canvas.style.cursor = h >= 0 ? 'pointer' : 'grab';
      showTip(h, e.clientX - r.left, e.clientY - r.top);
      redraw();
    }
  };
  const onUp = e => {
    canvas.classList.remove('drag');
    const was = drag;
    drag = null;
    if (was && !was.moved) {
      const r = canvas.getBoundingClientRect();
      const h = pick(e.clientX - r.left, e.clientY - r.top);
      if (h >= 0 && nodes[h].path) onOpen(nodes[h].path, e.metaKey || e.ctrlKey);
    }
  };
  let tipEl = null;
  const showTip = (i, x, y) => {
    if (!tipEl) { tipEl = document.createElement('div'); tipEl.className = 'graph-tip'; tip.append(tipEl) }
    if (i < 0) { tipEl.hidden = true; return }
    tipEl.hidden = false;
    tipEl.textContent = nodes[i].title + (data.aggregated ? ' — topic' : '');
    tipEl.style.left = x + 14 + 'px';
    tipEl.style.top = y + 10 + 'px';
  };
  const ro = new ResizeObserver(() => { resize(); redraw() });
  ro.observe(canvas);
  canvas.addEventListener('wheel', onWheel, { passive: false });
  canvas.addEventListener('pointerdown', onDown);
  canvas.addEventListener('pointermove', onMove);
  canvas.addEventListener('pointerup', onUp);
  canvas.addEventListener('dblclick', () => { fit(); redraw() });
  return {
    destroy() {
      ro.disconnect();
      cancelAnimationFrame(raf);
      canvas.removeEventListener('wheel', onWheel);
      tipEl?.remove();
    },
  };
}
