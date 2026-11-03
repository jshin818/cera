use crate::{normalize, words, Embedder, DIMS};

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "for", "with", "at", "by", "from", "is", "are", "was", "were", "be", "been",
    "it", "its", "this", "that", "these", "those", "as", "not", "no", "so", "do", "does", "did", "i", "you", "we", "they", "he", "she", "my", "our",
    "your", "their", "can", "will", "would", "should", "could", "has", "have", "had", "about", "into", "than", "then", "there", "here", "what",
    "which", "who", "how", "when", "where", "why", "also", "just", "more", "most", "some", "any", "all", "each", "very",
];

/// Signed feature hashing: unigrams (1.0), bigrams (0.5), character trigrams of words (0.25),
/// with sublinear term frequency. Deterministic and fast; the always-on CPU fallback.
#[derive(Default)]
pub struct HashEmbedder;

fn add(v: &mut [f32], feat: &str, w: f32) {
    let h = xxhash_rust::xxh3::xxh3_64(feat.as_bytes());
    let idx = (h % DIMS as u64) as usize;
    let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
    v[idx] += sign * w;
}

impl HashEmbedder {
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut counts: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
        let toks: Vec<String> = words(text).filter(|w| !STOP.contains(&w.as_str()) && w.chars().count() > 1).collect();
        for t in &toks {
            *counts.entry(format!("w:{t}")).or_default() += 1.0;
            let chars: Vec<char> = format!("<{t}>").chars().collect();
            if chars.len() > 5 {
                for tri in chars.windows(3) {
                    *counts.entry(format!("c:{}", tri.iter().collect::<String>())).or_default() += 0.25;
                }
            }
        }
        for pair in toks.windows(2) {
            *counts.entry(format!("b:{} {}", pair[0], pair[1])).or_default() += 0.5;
        }
        let mut v = vec![0f32; DIMS];
        for (f, c) in counts {
            add(&mut v, &f, 1.0 + c.ln_1p());
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
