//! Local embeddings and vector search. Nothing here touches the network.
//!
//! * [`StaticModel`]: Model2Vec format (potion-base-8M / potion-retrieval-32M): `model.safetensors`
//!   + `tokenizer.json`. Mean of token vectors, Matryoshka-truncated to 256 dims.
//! * [`HashEmbedder`]: always-available CPU fallback (signed feature hashing of words, bigrams and
//!   character trigrams). Lower quality, zero download.
//! * [`VectorIndex`]: binary codes (sign bits) for a Hamming coarse pass + int8 vectors for rescoring.

mod hashed;
mod index;
mod static_model;

pub use hashed::HashEmbedder;
pub use index::{VectorHit, VectorIndex};
pub use static_model::StaticModel;

pub const DIMS: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("model format: {0}")]
    Format(String),
}

pub type Result<T> = std::result::Result<T, EmbedError>;

pub trait Embedder: Send + Sync {
    /// Stable identifier stored with each chunk so a model change triggers re-embedding.
    fn name(&self) -> &str;
    /// Embed texts into L2-normalized `DIMS`-dimensional vectors.
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>>;
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Pick the best available embedder: a Model2Vec model in `models_dir` (first match of
/// potion-retrieval-32M, potion-base-8M, or any folder containing model.safetensors), else hashing.
pub fn best_available(models_dir: Option<&std::path::Path>) -> Box<dyn Embedder> {
    if let Some(dir) = models_dir {
        let mut cands = vec![dir.join("potion-retrieval-32M"), dir.join("potion-base-8M")];
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut extra: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.join("model.safetensors").exists()).collect();
            extra.sort();
            cands.extend(extra);
        }
        for c in cands {
            if c.join("model.safetensors").exists() {
                match StaticModel::load(&c) {
                    Ok(m) => {
                        tracing::info!(model = %m.name(), "loaded static embedding model");
                        return Box::new(m);
                    }
                    Err(e) => tracing::warn!(path = %c.display(), error = %e, "could not load model; trying next"),
                }
            }
        }
    }
    Box::new(HashEmbedder::default())
}

/// Tokens for the hashed embedder and keyword stats: lowercase words.
pub(crate) fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    use unicode_segmentation::UnicodeSegmentation;
    text.unicode_words().map(|w| w.to_lowercase())
}
