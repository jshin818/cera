//! Near-duplicate detection: MinHash signatures (128 permutations over 5-word shingles) with LSH
//! banding (32 bands x 4 rows); candidates are confirmed by estimated Jaccard >= threshold.

use std::collections::HashMap;

pub const PERMS: usize = 128;
const BANDS: usize = 32;
const ROWS: usize = PERMS / BANDS;

pub struct MinHasher {
    #[allow(dead_code)]
    a: [u64; PERMS],
    b: [u64; PERMS],
}

impl Default for MinHasher {
    fn default() -> Self {
        let mut a = [0u64; PERMS];
        let mut b = [0u64; PERMS];
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for i in 0..PERMS {
            s = splitmix(s);
            a[i] = s | 1;
            s = splitmix(s);
            b[i] = s;
        }
        MinHasher { a, b }
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl MinHasher {
    /// Signature of a text, or None if it has fewer than 5 words.
    pub fn signature(&self, text: &str) -> Option<[u32; PERMS]> {
        // Hash each normalized word once, then mix 5 word hashes per shingle (no allocation per shingle).
        let mut buf = String::new();
        let words: Vec<u64> = text
            .split_whitespace()
            .filter_map(|w| {
                let w = w.trim_matches(|c: char| !c.is_alphanumeric());
                if w.is_empty() {
                    return None;
                }
                buf.clear();
                buf.extend(w.chars().flat_map(char::to_lowercase));
                Some(xxhash_rust::xxh3::xxh3_64(buf.as_bytes()))
            })
            .collect();
        if words.len() < 5 {
            return None;
        }
        // One-permutation hashing: each shingle hashed once into one of PERMS bins (keep the min
        // per bin), then empty bins are densified from the next non-empty bin. O(shingles).
        let mut sig = [u32::MAX; PERMS];
        for sh in words.windows(5) {
            let h = splitmix(sh[0] ^ splitmix(sh[1] ^ splitmix(sh[2] ^ splitmix(sh[3] ^ splitmix(sh[4] ^ self.b[0])))));
            let bin = (h % PERMS as u64) as usize;
            let v = (h >> 32) as u32;
            if v < sig[bin] {
                sig[bin] = v;
            }
        }
        if sig.iter().all(|&v| v == u32::MAX) {
            return None;
        }
        for i in 0..PERMS {
            if sig[i] == u32::MAX {
                let mut j = (i + 1) % PERMS;
                let mut hops = 1u32;
                while sig[j] == u32::MAX {
                    j = (j + 1) % PERMS;
                    hops += 1;
                }
                // Rotation keeps densified values distinguishable per bin.
                sig[i] = sig[j].wrapping_add(hops.wrapping_mul(0x9E37_79B9)) | 1;
            }
        }
        Some(sig)
    }
}

pub fn jaccard_estimate(a: &[u32; PERMS], b: &[u32; PERMS]) -> f32 {
    a.iter().zip(b).filter(|(x, y)| x == y).count() as f32 / PERMS as f32
}

#[derive(Default)]
pub struct NearDupIndex {
    sigs: HashMap<u32, [u32; PERMS]>,
    bands: Vec<HashMap<u64, Vec<u32>>>,
}

impl NearDupIndex {
    pub fn new() -> Self {
        NearDupIndex { sigs: HashMap::new(), bands: (0..BANDS).map(|_| HashMap::new()).collect() }
    }

    fn band_key(sig: &[u32; PERMS], band: usize) -> u64 {
        let mut bytes = [0u8; ROWS * 4];
        for r in 0..ROWS {
            bytes[r * 4..r * 4 + 4].copy_from_slice(&sig[band * ROWS + r].to_le_bytes());
        }
        xxhash_rust::xxh3::xxh3_64(&bytes)
    }

    pub fn insert(&mut self, id: u32, sig: [u32; PERMS]) {
        self.remove(id);
        for band in 0..BANDS {
            self.bands[band].entry(Self::band_key(&sig, band)).or_default().push(id);
        }
        self.sigs.insert(id, sig);
    }

    pub fn remove(&mut self, id: u32) {
        if let Some(sig) = self.sigs.remove(&id) {
            for band in 0..BANDS {
                if let Some(v) = self.bands[band].get_mut(&Self::band_key(&sig, band)) {
                    v.retain(|&x| x != id);
                }
            }
        }
    }

    /// Notes that are near-duplicates of `id` (estimated Jaccard >= threshold), best first.
    pub fn near(&self, id: u32, threshold: f32) -> Vec<(u32, f32)> {
        let Some(sig) = self.sigs.get(&id) else { return Vec::new() };
        let mut cands = std::collections::HashSet::new();
        for band in 0..BANDS {
            if let Some(v) = self.bands[band].get(&Self::band_key(sig, band)) {
                cands.extend(v.iter().copied().filter(|&x| x != id));
            }
        }
        let mut out: Vec<(u32, f32)> = cands.into_iter().map(|c| (c, jaccard_estimate(sig, &self.sigs[&c]))).filter(|(_, j)| *j >= threshold).collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }

    /// All near-duplicate pairs (a < b).
    pub fn all_pairs(&self, threshold: f32) -> Vec<(u32, u32, f32)> {
        let mut out = Vec::new();
        let mut ids: Vec<u32> = self.sigs.keys().copied().collect();
        ids.sort();
        for &a in &ids {
            for (b, j) in self.near(a, threshold) {
                if a < b {
                    out.push((a, b, j));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_near_duplicates() {
        let h = MinHasher::default();
        let base: String = (0..200).map(|i| format!("word{} ", i % 97)).collect::<String>() + "the quick brown fox jumps over the lazy dog many times today";
        let mut near = base.clone();
        near.push_str(" plus one tiny extra sentence");
        let other: String = (0..200).map(|i| format!("other{} ", i)).collect();
        let mut idx = NearDupIndex::new();
        idx.insert(1, h.signature(&base).unwrap());
        idx.insert(2, h.signature(&near).unwrap());
        idx.insert(3, h.signature(&other).unwrap());
        let r = idx.near(1, 0.8);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 2);
        assert!(h.signature("too short").is_none());
    }
}
