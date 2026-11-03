use crate::{normalize, words, Embedder, DIMS};

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "for", "with", "at", "by", "from", "is", "are", "was", "were", "be", "been",
    "it", "its", "this", "that", "these", "those", "as", "not", "no", "so", "do", "does", "did", "i", "you", "we", "they", "he", "she", "my", "our",
    "your", "their", "can", "will", "would", "should", "could", "has", "have", "had", "about", "into", "than", "then", "there", "here", "what",
    "which", "who", "how", "when", "where", "why", "also", "just", "more", "most", "some", "any", "all", "each", "very",
];

/// Signed feature hashing: unigrams (1.0), bigrams (0.5), character trigrams of longer words
/// (0.25). Features are hashed straight from bytes (no allocation per feature). Deterministic and
/// fast; the always-on CPU fallback when no Model2Vec model is installed.
#[derive(Default)]
pub struct HashEmbedder;

#[inline]
fn add(v: &mut [f32], h: u64, w: f32) {
    let idx = (h % DIMS as u64) as usize;
    v[idx] += if (h >> 63) & 1 == 0 { w } else { -w };
}

#[inline]
fn mix(a: u64, b: u64) -> u64 {
    let x = a.rotate_left(23) ^ b.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 29)
}

impl HashEmbedder {
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; DIMS];
        let mut buf = String::new();
        let mut prev: Option<u64> = None;
        for w in words(text) {
            if w.chars().count() <= 1 || STOP.contains(&w.as_str()) {
                continue;
            }
            let h = xxhash_rust::xxh3::xxh3_64_with_seed(w.as_bytes(), 1);
            add(&mut v, h, 1.0);
            if let Some(p) = prev {
                add(&mut v, mix(p, h), 0.5);
            }
            prev = Some(h);
            if w.len() > 4 {
                buf.clear();
                buf.push('<');
                buf.push_str(&w);
                buf.push('>');
                let b = buf.as_bytes();
                for tri in b.windows(3) {
                    add(&mut v, xxhash_rust::xxh3::xxh3_64_with_seed(tri, 3), 0.25);
                }
            }
        }
        // Sublinear dampening of repeated features, then L2 normalisation.
        for x in v.iter_mut() {
            *x = x.signum() * x.abs().sqrt();
        }
        normalize(&mut v);
        v
    }
}

impl Embedder for HashEmbedder {
    fn name(&self) -> &str {
        "hash-256-v1"
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        use rayon::prelude::*;
        texts.par_iter().map(|t| self.embed_one(t)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dot;

    #[test]
    fn similar_texts_score_higher() {
        let e = HashEmbedder;
        let v = e.embed(&["rust ownership and borrowing rules", "borrowing rules in rust ownership model", "tonkotsu ramen broth recipe"]);
        assert!(dot(&v[0], &v[1]) > dot(&v[0], &v[2]) + 0.2);
        assert!((dot(&v[0], &v[0]) - 1.0).abs() < 1e-5);
    }
}
