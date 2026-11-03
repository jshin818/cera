//! Model2Vec static embeddings: a token embedding matrix plus a WordPiece or Unigram vocab.
//! Embedding = mean of token vectors (optionally weighted), L2-normalized, truncated to 256 dims.

use crate::{normalize, EmbedError, Embedder, Result, DIMS};
use std::collections::HashMap;
use std::path::Path;

pub struct StaticModel {
    name: String,
    dim: usize,
    matrix: Vec<f32>,
    vocab: HashMap<String, u32>,
    unk: Option<u32>,
    wordpiece: bool,
    lowercase: bool,
    max_chars: usize,
}

impl StaticModel {
    pub fn load(dir: &Path) -> Result<StaticModel> {
        let bytes = std::fs::read(dir.join("model.safetensors"))?;
        let st = safetensors::SafeTensors::deserialize(&bytes).map_err(|e| EmbedError::Format(e.to_string()))?;
        let names = st.names();
        let tname = if names.iter().any(|n| *n == "embeddings") { "embeddings".to_string() } else { names.first().map(|s| s.to_string()).ok_or_else(|| EmbedError::Format("no tensors".into()))? };
        let t = st.tensor(&tname).map_err(|e| EmbedError::Format(e.to_string()))?;
        let shape = t.shape();
        if shape.len() != 2 {
            return Err(EmbedError::Format(format!("expected 2-D embeddings, got {shape:?}")));
        }
        let (rows, full_dim) = (shape[0], shape[1]);
        let raw = t.data();
        let full: Vec<f32> = match t.dtype() {
            safetensors::Dtype::F32 => raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
            safetensors::Dtype::F16 => raw.chunks_exact(2).map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]]))).collect(),
            safetensors::Dtype::BF16 => raw.chunks_exact(2).map(|c| f32::from_bits((u16::from_le_bytes([c[0], c[1]]) as u32) << 16)).collect(),
            d => return Err(EmbedError::Format(format!("unsupported dtype {d:?}"))),
        };
        // Matryoshka-style truncation to DIMS (potion models are PCA-ordered, so leading dims carry the most variance).
        let dim = full_dim.min(DIMS);
        let mut matrix = Vec::with_capacity(rows * DIMS);
        for r in 0..rows {
            matrix.extend_from_slice(&full[r * full_dim..r * full_dim + dim]);
            matrix.extend(std::iter::repeat_n(0.0, DIMS - dim));
        }
        let tok: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("tokenizer.json"))?).map_err(|e| EmbedError::Format(e.to_string()))?;
        let model = &tok["model"];
        let mut vocab = HashMap::new();
        let wordpiece = model["type"].as_str() != Some("Unigram");
        match &model["vocab"] {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    if let Some(id) = v.as_u64() {
                        vocab.insert(k.clone(), id as u32);
                    }
                }
            }
            serde_json::Value::Array(a) => {
                for (i, e) in a.iter().enumerate() {
                    if let Some(s) = e.get(0).and_then(|s| s.as_str()) {
                        vocab.insert(s.to_string(), i as u32);
                    }
                }
            }
            _ => return Err(EmbedError::Format("tokenizer.json has no vocab".into())),
        }
        let unk_tok = model["unk_token"].as_str().unwrap_or("[UNK]");
        let unk = vocab.get(unk_tok).copied();
        let lowercase = tok["normalizer"]["lowercase"].as_bool().unwrap_or(true);
        let name = format!("m2v:{}", dir.file_name().and_then(|s| s.to_str()).unwrap_or("model"));
        vocab.retain(|_, id| (*id as usize) < rows);
        Ok(StaticModel { name, dim: DIMS, matrix, vocab, unk, wordpiece, lowercase, max_chars: 100 })
    }

    fn token_ids(&self, text: &str) -> Vec<u32> {
        let text = if self.lowercase { text.to_lowercase() } else { text.to_string() };
        let mut ids = Vec::new();
        for word in pre_tokenize(&text) {
            if word.chars().count() > self.max_chars {
                continue;
            }
            if self.wordpiece {
                self.wordpiece_ids(word, &mut ids);
            } else if let Some(&id) = self.vocab.get(&format!("▁{word}")).or_else(|| self.vocab.get(word)) {
                ids.push(id);
            } else {
                self.greedy_pieces(word, "▁", "", &mut ids);
            }
        }
        ids
    }

    fn wordpiece_ids(&self, word: &str, out: &mut Vec<u32>) {
        if let Some(&id) = self.vocab.get(word) {
            out.push(id);
            return;
        }
        self.greedy_pieces(word, "", "##", out);
    }

    /// Greedy longest-match-first subword split (WordPiece style).
    fn greedy_pieces(&self, word: &str, first_prefix: &str, cont_prefix: &str, out: &mut Vec<u32>) {
        let chars: Vec<(usize, char)> = word.char_indices().collect();
        let mut start = 0;
        let mut pieces = Vec::new();
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while end > start {
                let s = chars[start].0;
                let e = if end == chars.len() { word.len() } else { chars[end].0 };
                let pre = if start == 0 { first_prefix } else { cont_prefix };
                if let Some(&id) = self.vocab.get(&format!("{pre}{}", &word[s..e])) {
                    found = Some(id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => {
                    pieces.push(id);
                    start = end;
                }
                None => {
                    if let Some(u) = self.unk {
                        out.push(u);
                    }
                    return;
                }
            }
        }
        out.extend(pieces);
    }
}

fn pre_tokenize(text: &str) -> Vec<&str> {
    // BERT-style: split on whitespace and around punctuation.
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push(&text[s..i]);
            }
        } else if c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace()) {
            if let Some(s) = start.take() {
                out.push(&text[s..i]);
            }
            out.push(&text[i..i + c.len_utf8()]);
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    out
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if frac == 0 => sign,
        0 => {
            // subnormal
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

impl Embedder for StaticModel {
    fn name(&self) -> &str {
        &self.name
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        use rayon::prelude::*;
        texts
            .par_iter()
            .map(|t| {
                let ids = self.token_ids(t);
                let mut v = vec![0f32; self.dim];
                for &id in &ids {
                    let row = &self.matrix[id as usize * self.dim..(id as usize + 1) * self.dim];
                    v.iter_mut().zip(row).for_each(|(a, b)| *a += b);
                }
                normalize(&mut v);
                v
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny WordPiece model on disk in Model2Vec layout.
    fn tiny_model(dir: &Path) {
        let vocab = ["[UNK]", "rust", "own", "##ership", "ramen", "broth", "memory"];
        let dim = 8usize;
        let mut data = Vec::new();
        for (i, _) in vocab.iter().enumerate() {
            for d in 0..dim {
                let x: f32 = match i {
                    1..=3 | 6 => if d < 3 { 1.0 } else { 0.0 },
                    4 | 5 => if d >= 5 { 1.0 } else { 0.0 },
                    _ => 0.01,
                };
                data.extend_from_slice(&x.to_le_bytes());
            }
        }
        let view = safetensors::tensor::TensorView::new(safetensors::Dtype::F32, vec![vocab.len(), dim], &data).unwrap();
        safetensors::serialize_to_file([("embeddings", view)], None, &dir.join("model.safetensors")).unwrap();
        let v: serde_json::Map<String, serde_json::Value> = vocab.iter().enumerate().map(|(i, t)| (t.to_string(), serde_json::json!(i))).collect();
        std::fs::write(dir.join("tokenizer.json"), serde_json::json!({"model": {"type": "WordPiece", "unk_token": "[UNK]", "vocab": v}, "normalizer": {"lowercase": true}}).to_string()).unwrap();
    }

    #[test]
    fn loads_and_embeds() {
        let d = tempfile::tempdir().unwrap();
        tiny_model(d.path());
        let m = StaticModel::load(d.path()).unwrap();
        assert_eq!(m.token_ids("Rust ownership!"), vec![1, 2, 3, 0]);
        let v = m.embed(&["rust ownership", "memory", "ramen broth"]);
        assert!(crate::dot(&v[0], &v[1]) > 0.9);
        assert!(crate::dot(&v[0], &v[2]) < 0.1);
        assert_eq!(v[0].len(), DIMS);
        let best = crate::best_available(Some(d.path().parent().unwrap()));
        let _ = best.name();
    }

    #[test]
    fn f16_conversion() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0), 0.0);
    }
}
