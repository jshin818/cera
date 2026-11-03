/// Compressed sparse row graph. Keeps both the directed out-edges (for PageRank) and an
/// undirected, deduplicated, weighted view (for PPR, clustering, link prediction and layout).
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub n: usize,
    out_off: Vec<u32>,
    out_dst: Vec<u32>,
    und_off: Vec<u32>,
    und_dst: Vec<u32>,
    und_w: Vec<f32>,
}

impl Graph {
    /// `edges` are directed (src, dst); self-loops are dropped, duplicates merge into weights.
    pub fn new(n: usize, edges: &[(u32, u32)]) -> Graph {
        let mut out: Vec<(u32, u32)> = edges.iter().copied().filter(|&(a, b)| a != b && (a as usize) < n && (b as usize) < n).collect();
        out.sort_unstable();
        out.dedup();
        let (out_off, out_dst) = build(n, out.iter().map(|&(a, b)| (a, b, 1.0)));
        let mut und: Vec<(u32, u32)> = out.iter().flat_map(|&(a, b)| [(a, b), (b, a)]).collect();
        und.sort_unstable();
        let mut merged: Vec<(u32, u32, f32)> = Vec::with_capacity(und.len());
        for (a, b) in und {
            match merged.last_mut() {
                Some(l) if l.0 == a && l.1 == b => l.2 += 1.0,
                _ => merged.push((a, b, 1.0)),
            }
        }
        let und_w = merged.iter().map(|e| e.2).collect();
        let (und_off, und_dst) = build(n, merged.into_iter());
        Graph { n, out_off, out_dst, und_off, und_dst, und_w }
    }

    pub fn from_weighted_undirected(n: usize, edges: &[(u32, u32, f32)]) -> Graph {
        let mut e: Vec<(u32, u32, f32)> = edges.iter().flat_map(|&(a, b, w)| [(a, b, w), (b, a, w)]).filter(|e| e.0 != e.1).collect();
        e.sort_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
        let mut merged: Vec<(u32, u32, f32)> = Vec::with_capacity(e.len());
        for x in e {
            match merged.last_mut() {
                Some(l) if l.0 == x.0 && l.1 == x.1 => l.2 += x.2,
                _ => merged.push(x),
            }
        }
        let und_w = merged.iter().map(|e| e.2).collect();
        let (und_off, und_dst) = build(n, merged.into_iter());
        Graph { n, out_off: vec![0; n + 1], out_dst: Vec::new(), und_off, und_dst, und_w }
    }

    #[inline]
    pub fn out(&self, v: u32) -> &[u32] {
        &self.out_dst[self.out_off[v as usize] as usize..self.out_off[v as usize + 1] as usize]
    }

    #[inline]
    pub fn neighbors(&self, v: u32) -> &[u32] {
        &self.und_dst[self.und_off[v as usize] as usize..self.und_off[v as usize + 1] as usize]
    }

    #[inline]
    pub fn weights(&self, v: u32) -> &[f32] {
        &self.und_w[self.und_off[v as usize] as usize..self.und_off[v as usize + 1] as usize]
    }

    #[inline]
    pub fn degree(&self, v: u32) -> usize {
        (self.und_off[v as usize + 1] - self.und_off[v as usize]) as usize
    }

    pub fn weighted_degree(&self, v: u32) -> f32 {
        self.weights(v).iter().sum()
    }

    pub fn total_weight(&self) -> f32 {
        self.und_w.iter().sum::<f32>() / 2.0
    }

    pub fn edge_count(&self) -> usize {
        self.out_dst.len()
    }

    pub fn has_edge(&self, a: u32, b: u32) -> bool {
        self.neighbors(a).binary_search(&b).is_ok()
    }

    /// Undirected edges (a < b) with weights.
    pub fn undirected_edges(&self) -> impl Iterator<Item = (u32, u32, f32)> + '_ {
        (0..self.n as u32).flat_map(move |a| self.neighbors(a).iter().zip(self.weights(a)).filter(move |(b, _)| **b > a).map(move |(&b, &w)| (a, b, w)))
    }
}

fn build(n: usize, sorted: impl Iterator<Item = (u32, u32, f32)>) -> (Vec<u32>, Vec<u32>) {
    let mut off = vec![0u32; n + 1];
    let mut dst = Vec::new();
    for (a, b, _) in sorted {
        off[a as usize + 1] += 1;
        dst.push(b);
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    (off, dst)
}
