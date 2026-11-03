use crate::Graph;
use rayon::prelude::*;
use std::collections::HashMap;

/// Global PageRank by power iteration on out-links. Dangling mass is spread uniformly.
/// Returns scores summing to 1. `active` marks real nodes (freed ids get 0).
pub fn pagerank(g: &Graph, active: &[bool], damping: f32, max_iter: usize, tol: f32) -> Vec<f32> {
    let n_active = active.iter().filter(|a| **a).count().max(1) as f32;
    let mut pr: Vec<f32> = active.iter().map(|&a| if a { 1.0 / n_active } else { 0.0 }).collect();
    let outdeg: Vec<f32> = (0..g.n as u32).map(|v| g.out(v).len() as f32).collect();
    // Precompute in-edges for a pull-based parallel iteration.
    let mut in_lists: Vec<Vec<u32>> = vec![Vec::new(); g.n];
    for v in 0..g.n as u32 {
        for &u in g.out(v) {
            in_lists[u as usize].push(v);
        }
    }
    for _ in 0..max_iter {
        let dangling: f32 = (0..g.n).filter(|&v| active[v] && outdeg[v] == 0.0).map(|v| pr[v]).sum();
        let base = (1.0 - damping) / n_active + damping * dangling / n_active;
        let next: Vec<f32> = (0..g.n)
            .into_par_iter()
            .map(|v| if !active[v] { 0.0 } else { base + damping * in_lists[v].iter().map(|&u| pr[u as usize] / outdeg[u as usize]).sum::<f32>() })
            .collect();
        let diff: f32 = next.iter().zip(&pr).map(|(a, b)| (a - b).abs()).sum();
        pr = next;
        if diff < tol {
            break;
        }
    }
    pr
}

/// Approximate personalized PageRank by forward push (Andersen-Chung-Lang) on the undirected
/// view. `alpha` is the teleport probability, `eps` the residual threshold per unit degree.
pub fn personalized_pagerank(g: &Graph, seeds: &[u32], alpha: f32, eps: f32) -> HashMap<u32, f32> {
    let mut p: HashMap<u32, f32> = HashMap::new();
    let mut r: HashMap<u32, f32> = HashMap::new();
    if seeds.is_empty() {
        return p;
    }
    let w = 1.0 / seeds.len() as f32;
    let mut queue: Vec<u32> = Vec::new();
    for &s in seeds {
        *r.entry(s).or_default() += w;
        queue.push(s);
    }
    let mut pushes = 0usize;
    while let Some(u) = queue.pop() {
        let ru = *r.get(&u).unwrap_or(&0.0);
        let du = g.degree(u).max(1) as f32;
        if ru < eps * du {
            continue;
        }
        pushes += 1;
        if pushes > 2_000_000 {
            break;
        }
        *p.entry(u).or_default() += alpha * ru;
        let spread = (1.0 - alpha) * ru;
        r.insert(u, 0.0);
        let nb = g.neighbors(u);
        if nb.is_empty() {
            *p.entry(u).or_default() += spread;
            continue;
        }
        let share = spread / nb.len() as f32;
        for &v in nb {
            let rv = r.entry(v).or_default();
            let before = *rv;
            *rv += share;
            let dv = g.degree(v).max(1) as f32;
            if before < eps * dv && *rv >= eps * dv {
                queue.push(v);
            }
        }
    }
    p
}

/// Adamic-Adar link prediction from `v`: score(v, x) = sum over common neighbours z of 1/ln(deg z),
/// for 2-hop nodes x not already adjacent to v. Returns the top `k` by score.
pub fn adamic_adar(g: &Graph, v: u32, k: usize) -> Vec<(u32, f32)> {
    let mut acc: HashMap<u32, f32> = HashMap::new();
    for &z in g.neighbors(v) {
        let dz = g.degree(z);
        if dz < 2 {
            continue;
        }
        let w = 1.0 / (dz as f32).ln().max(0.1);
        for &x in g.neighbors(z) {
            if x != v && !g.has_edge(v, x) {
                *acc.entry(x).or_default() += w;
            }
        }
    }
    let mut out: Vec<(u32, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(k);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_ranks_highest() {
        let edges: Vec<(u32, u32)> = (1..6).map(|i| (i, 0)).chain([(0, 1)]).collect();
        let g = Graph::new(6, &edges);
        let pr = pagerank(&g, &[true; 6], 0.85, 30, 1e-6);
        assert!((pr.iter().sum::<f32>() - 1.0).abs() < 1e-3);
        assert!(pr[0] > pr[1] && pr[1] > pr[2]);
    }

    #[test]
    fn ppr_prefers_near_nodes() {
        // path 0-1-2-3-4
        let g = Graph::new(5, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
        let p = personalized_pagerank(&g, &[0], 0.15, 1e-6);
        assert!(p[&1] > p[&2] && p[&2] > p[&3]);
    }

    #[test]
    fn adamic_adar_suggests_triangle_closure() {
        // 0-1, 0-2, 1-3, 2-3 : 0 and 3 share neighbours 1 and 2
        let g = Graph::new(5, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)]);
        let s = adamic_adar(&g, 0, 3);
        assert_eq!(s[0].0, 3);
    }
}
