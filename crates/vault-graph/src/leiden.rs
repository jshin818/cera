//! Leiden community detection (Traag, Waltman & van Eck 2019), modularity objective.
//! Phases: fast local moving -> refinement (communities split into well-connected sub-communities,
//! guaranteeing connectivity) -> aggregation on the refined partition, seeded with the unrefined one.

use crate::Graph;
use std::collections::{HashMap, VecDeque};

/// Returns a community id per node, dense from 0, ordered by community size (largest first).
pub fn leiden(g: &Graph, resolution: f32, max_levels: usize) -> Vec<u32> {
    let n = g.n;
    if n == 0 {
        return Vec::new();
    }
    let mut node_to_agg: Vec<u32> = (0..n as u32).collect(); // original node -> current aggregate node
    let mut graph = g.clone();
    let mut node_w: Vec<f32> = (0..n as u32).map(|v| g.weighted_degree(v)).collect();
    let m2 = node_w.iter().sum::<f32>().max(1e-9);
    let mut part: Vec<u32> = (0..n as u32).collect();
    for _level in 0..max_levels {
        let moved = local_moving(&graph, &node_w, &mut part, resolution, m2);
        let refined = refine(&graph, &node_w, &part, resolution, m2);
        let n_ref = 1 + *refined.iter().max().unwrap_or(&0) as usize;
        if !moved && n_ref == graph.n {
            break;
        }
        // Aggregate on refined communities.
        let mut edges: HashMap<(u32, u32), f32> = HashMap::new();
        for a in 0..graph.n as u32 {
            for (&b, &w) in graph.neighbors(a).iter().zip(graph.weights(a)) {
                let (ca, cb) = (refined[a as usize], refined[b as usize]);
                // Each undirected edge appears in both directions; count it once. Internal edges
                // live on in the aggregate node weight.
                if ca < cb {
                    *edges.entry((ca, cb)).or_default() += w;
                }
            }
        }
        let mut new_w = vec![0f32; n_ref];
        let mut agg_part = vec![0u32; n_ref];
        for v in 0..graph.n {
            new_w[refined[v] as usize] += node_w[v];
            agg_part[refined[v] as usize] = part[v];
        }
        let e: Vec<(u32, u32, f32)> = edges.into_iter().map(|((a, b), w)| (a, b, w)).collect();
        for x in node_to_agg.iter_mut() {
            *x = refined[*x as usize];
        }
        if n_ref == graph.n {
            break;
        }
        graph = Graph::from_weighted_undirected(n_ref, &e);
        node_w = new_w;
        part = agg_part;
    }
    // Map original nodes to final communities and relabel by size.
    let raw: Vec<u32> = node_to_agg.iter().map(|&a| part[a as usize]).collect();
    let fixed = split_disconnected(g, &raw);
    relabel_by_size(&fixed)
}

fn local_moving(g: &Graph, w: &[f32], part: &mut [u32], gamma: f32, m2: f32) -> bool {
    let n = g.n;
    let mut comm_w: HashMap<u32, f32> = HashMap::new();
    for v in 0..n {
        *comm_w.entry(part[v]).or_default() += w[v];
    }
    let mut queue: VecDeque<u32> = (0..n as u32).collect();
    let mut in_q = vec![true; n];
    let mut moved_any = false;
    let mut links: HashMap<u32, f32> = HashMap::new();
    while let Some(v) = queue.pop_front() {
        in_q[v as usize] = false;
        let cv = part[v as usize];
        links.clear();
        for (&u, &ew) in g.neighbors(v).iter().zip(g.weights(v)) {
            *links.entry(part[u as usize]).or_default() += ew;
        }
        let kv = w[v as usize];
        *comm_w.get_mut(&cv).unwrap() -= kv;
        let gain = |c: u32, l: f32, cw: &HashMap<u32, f32>| l - gamma * kv * cw.get(&c).copied().unwrap_or(0.0) / m2;
        let mut best = cv;
        let mut best_gain = gain(cv, links.get(&cv).copied().unwrap_or(0.0), &comm_w);
        let mut cands: Vec<(&u32, &f32)> = links.iter().collect();
        cands.sort_by_key(|(c, _)| **c);
        for (&c, &l) in cands {
            let gn = gain(c, l, &comm_w);
            if gn > best_gain + 1e-7 {
                best_gain = gn;
                best = c;
            }
        }
        *comm_w.entry(best).or_default() += kv;
        if best != cv {
            part[v as usize] = best;
            moved_any = true;
            for &u in g.neighbors(v) {
                if part[u as usize] != best && !in_q[u as usize] {
                    in_q[u as usize] = true;
                    queue.push_back(u);
                }
            }
        }
    }
    moved_any
}

/// Refinement: inside each community, start from singletons and merge a node into the
/// neighbouring sub-community with the best positive gain, only if the node is still a singleton
/// and well connected to its community. Sub-communities are connected by construction.
fn refine(g: &Graph, w: &[f32], part: &[u32], gamma: f32, m2: f32) -> Vec<u32> {
    let n = g.n;
    let mut sub: Vec<u32> = (0..n as u32).collect();
    let mut sub_w: Vec<f32> = w.to_vec();
    let mut sub_size = vec![1u32; n];
    let mut comm_tot: HashMap<u32, f32> = HashMap::new();
    for v in 0..n {
        *comm_tot.entry(part[v]).or_default() += w[v];
    }
    // Well-connectedness: links from v into its own community vs expectation.
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&v| (part[v as usize], v));
    let mut links: HashMap<u32, f32> = HashMap::new();
    for v in order {
        if sub_size[sub[v as usize] as usize] != 1 {
            continue;
        }
        let c = part[v as usize];
        let kv = w[v as usize];
        let tot = comm_tot[&c];
        let into_comm: f32 = g.neighbors(v).iter().zip(g.weights(v)).filter(|(u, _)| part[**u as usize] == c).map(|(_, x)| *x).sum();
        if into_comm < gamma * kv * (tot - kv) / m2 {
            continue;
        }
        links.clear();
        for (&u, &ew) in g.neighbors(v).iter().zip(g.weights(v)) {
            if part[u as usize] == c && u != v {
                *links.entry(sub[u as usize]).or_default() += ew;
            }
        }
        let mut best = sub[v as usize];
        let mut best_gain = 0.0f32;
        let mut cands: Vec<(&u32, &f32)> = links.iter().collect();
        cands.sort_by_key(|(s, _)| **s);
        for (&s, &l) in cands {
            if s == sub[v as usize] {
                continue;
            }
            let gn = l - gamma * kv * sub_w[s as usize] / m2;
            if gn > best_gain + 1e-7 {
                best_gain = gn;
                best = s;
            }
        }
        if best != sub[v as usize] {
            let old = sub[v as usize] as usize;
            sub_w[old] -= kv;
            sub_size[old] -= 1;
            sub[v as usize] = best;
            sub_w[best as usize] += kv;
            sub_size[best as usize] += 1;
        }
    }
    let mut map: HashMap<u32, u32> = HashMap::new();
    sub.iter()
        .map(|s| {
            let l = map.len() as u32;
            *map.entry(*s).or_insert(l)
        })
        .collect()
}

fn split_disconnected(g: &Graph, part: &[u32]) -> Vec<u32> {
    let n = g.n;
    let mut out = vec![u32::MAX; n];
    let mut next = 0u32;
    let mut stack = Vec::new();
    for s in 0..n {
        if out[s] != u32::MAX {
            continue;
        }
        out[s] = next;
        stack.push(s as u32);
        while let Some(v) = stack.pop() {
            for &u in g.neighbors(v) {
                if out[u as usize] == u32::MAX && part[u as usize] == part[s] {
                    out[u as usize] = next;
                    stack.push(u);
                }
            }
        }
        next += 1;
    }
    out
}

fn relabel_by_size(part: &[u32]) -> Vec<u32> {
    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for &c in part {
        *sizes.entry(c).or_default() += 1;
    }
    let mut order: Vec<(u32, usize)> = sizes.into_iter().collect();
    order.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let map: HashMap<u32, u32> = order.iter().enumerate().map(|(i, (c, _))| (*c, i as u32)).collect();
    part.iter().map(|c| map[c]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_cliques_with_a_bridge() {
        let mut e = Vec::new();
        for a in 0..5u32 {
            for b in a + 1..5 {
                e.push((a, b));
                e.push((a + 5, b + 5));
            }
        }
        e.push((4, 5));
        let g = Graph::new(11, &e); // node 10 isolated
        let c = leiden(&g, 1.0, 10);
        assert!((0..5).all(|i| c[i] == c[0]));
        assert!((5..10).all(|i| c[i] == c[5]));
        assert_ne!(c[0], c[5]);
        assert_ne!(c[10], c[0]);
        assert_ne!(c[10], c[5]);
    }

    #[test]
    fn communities_are_connected() {
        // ring of 6 triangles
        let mut e = Vec::new();
        for t in 0..6u32 {
            let b = t * 3;
            e.extend([(b, b + 1), (b + 1, b + 2), (b, b + 2), (b + 2, (b + 3) % 18)]);
        }
        let g = Graph::new(18, &e);
        let c = leiden(&g, 1.0, 10);
        let k = *c.iter().max().unwrap() as usize + 1;
        assert!((2..=6).contains(&k), "{k}");
        // each community connected
        for cid in 0..k as u32 {
            let nodes: Vec<u32> = (0..18).filter(|&v| c[v as usize] == cid).collect();
            let mut seen = vec![nodes[0]];
            let mut i = 0;
            while i < seen.len() {
                for &u in g.neighbors(seen[i]) {
                    if c[u as usize] == cid && !seen.contains(&u) {
                        seen.push(u);
                    }
                }
                i += 1;
            }
            assert_eq!(seen.len(), nodes.len());
        }
    }
}
