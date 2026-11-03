//! Reciprocal Rank Fusion: calibration-free merging of ranked lists.

use std::collections::HashMap;

pub const RRF_K: f32 = 60.0;

/// `lists` are ranked id lists (best first). Returns (id, rrf score) sorted by score descending,
/// ties broken by id for determinism.
pub fn rrf(lists: &[&[u32]]) -> Vec<(u32, f32)> {
    let mut acc: HashMap<u32, f32> = HashMap::new();
    for list in lists {
        for (rank, &id) in list.iter().enumerate() {
            *acc.entry(id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }
    let mut v: Vec<_> = acc.into_iter().collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_wins() {
        let a = [1, 2, 3];
        let b = [3, 1, 4];
        let r = rrf(&[&a, &b]);
        assert_eq!(r[0].0, 1);
        assert_eq!(r[1].0, 3);
        assert_eq!(r.len(), 4);
    }
}
