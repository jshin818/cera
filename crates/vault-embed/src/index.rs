//! Binary-quantized brute-force vector search with int8 rescoring.
//!
//! Each chunk vector is stored twice: sign bits (DIMS/64 u64 words, Hamming coarse pass) and int8
//! with a per-vector scale (rescoring of the top `rescore` candidates). At 256 dims that is 32 bytes
//! + 260 bytes per chunk.

use crate::{DIMS, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

const WORDS: usize = DIMS / 64;
const MAGIC: &[u8; 4] = b"VVE2";
const OWNED: u32 = 1 << 31;

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct VectorHit {
    pub file_id: u32,
    pub chunk: u32,
    pub score: f32,
}

/// Binary codes, keys and scales live in RAM (~44 bytes per chunk). The int8 rescoring vectors
/// live in a memory-mapped file (`q8.bin`) once saved, so only rescored candidates are paged in;
/// rows written since the last save are held in `q8_owned`.
#[derive(Default)]
pub struct VectorIndex {
    pub model: String,
    keys: Vec<(u32, u32)>,
    bits: Vec<[u64; WORDS]>,
    /// Per row: index into the mapped file, or `OWNED | index` into `q8_owned`.
    q8_src: Vec<u32>,
    q8_owned: Vec<[i8; DIMS]>,
    q8_map: Option<std::sync::Arc<memmap2::Mmap>>,
    scale: Vec<f32>,
    /// file_id -> row indices
    rows: HashMap<u32, Vec<usize>>,
    /// rows freed by removals, reused on insert
    free: Vec<usize>,
}

fn quantize(v: &[f32]) -> ([u64; WORDS], [i8; DIMS], f32) {
    let mut bits = [0u64; WORDS];
    for (i, &x) in v.iter().enumerate().take(DIMS) {
        if x > 0.0 {
            bits[i / 64] |= 1 << (i % 64);
        }
    }
    let max = v.iter().fold(0f32, |m, x| m.max(x.abs())).max(1e-12);
    let s = max / 127.0;
    let mut q = [0i8; DIMS];
    for (i, &x) in v.iter().enumerate().take(DIMS) {
        q[i] = (x / s).round().clamp(-127.0, 127.0) as i8;
    }
    (bits, q, s)
}

impl VectorIndex {
    pub fn new(model: &str) -> Self {
        VectorIndex { model: model.to_string(), ..Default::default() }
    }

    #[inline]
    fn q8(&self, r: usize) -> &[i8; DIMS] {
        let src = self.q8_src[r];
        if src & OWNED != 0 {
            &self.q8_owned[(src & !OWNED) as usize]
        } else {
            let m = self.q8_map.as_ref().expect("mapped q8");
            let off = src as usize * DIMS;
            // SAFETY: i8 and u8 have the same layout; the slice is DIMS long and in bounds (checked on load).
            unsafe { &*(m[off..off + DIMS].as_ptr() as *const [i8; DIMS]) }
        }
    }

    fn put_q8(&mut self, r: Option<usize>, q: [i8; DIMS]) -> u32 {
        // Reuse an owned slot when overwriting an owned row.
        if let Some(r) = r {
            let src = self.q8_src[r];
            if src & OWNED != 0 {
                self.q8_owned[(src & !OWNED) as usize] = q;
                return src;
            }
        }
        self.q8_owned.push(q);
        OWNED | (self.q8_owned.len() - 1) as u32
    }

    pub fn len(&self) -> usize {
        self.keys.len() - self.free.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn has_file(&self, file_id: u32) -> bool {
        self.rows.contains_key(&file_id)
    }

    pub fn remove_file(&mut self, file_id: u32) {
        if let Some(rows) = self.rows.remove(&file_id) {
            for r in rows {
                self.keys[r] = (u32::MAX, u32::MAX);
                self.bits[r] = [0; WORDS];
                self.scale[r] = 0.0;
                self.free.push(r);
            }
        }
    }

    /// Replace all chunk vectors of a file.
    pub fn upsert_file(&mut self, file_id: u32, vecs: &[Vec<f32>]) {
        self.remove_file(file_id);
        let mut rows = Vec::with_capacity(vecs.len());
        for (ci, v) in vecs.iter().enumerate() {
            let (b, q, s) = quantize(v);
            let r = if let Some(r) = self.free.pop() {
                self.keys[r] = (file_id, ci as u32);
                self.bits[r] = b;
                self.q8_src[r] = self.put_q8(Some(r), q);
                self.scale[r] = s;
                r
            } else {
                self.keys.push((file_id, ci as u32));
                self.bits.push(b);
                let src = self.put_q8(None, q);
                self.q8_src.push(src);
                self.scale.push(s);
                self.keys.len() - 1
            };
            rows.push(r);
        }
        self.rows.insert(file_id, rows);
    }

    /// Dequantized mean of a file's chunk vectors (normalized), for "related notes".
    pub fn file_vector(&self, file_id: u32) -> Option<Vec<f32>> {
        let rows = self.rows.get(&file_id)?;
        let mut v = vec![0f32; DIMS];
        for &r in rows {
            for (a, &q) in v.iter_mut().zip(self.q8(r).iter()) {
                *a += q as f32 * self.scale[r];
            }
        }
        crate::normalize(&mut v);
        Some(v)
    }

    /// Dequantized per-chunk vectors of a file, in chunk order (for reusing unchanged chunks).
    pub fn file_vector_rows(&self, file_id: u32) -> Option<Vec<Vec<f32>>> {
        let rows = self.rows.get(&file_id)?;
        let mut out: Vec<(u32, Vec<f32>)> = rows.iter().map(|&r| (self.keys[r].1, self.q8(r).iter().map(|&q| q as f32 * self.scale[r]).collect())).collect();
        out.sort_by_key(|x| x.0);
        Some(out.into_iter().map(|x| x.1).collect())
    }

    pub fn files(&self) -> impl Iterator<Item = u32> + '_ {
        self.rows.keys().copied()
    }

    /// Top-k chunks by cosine similarity: Hamming coarse pass keeps `rescore` candidates, which are
    /// rescored with int8 dot products. `exclude` filters a file (e.g. the note itself).
    pub fn search(&self, query: &[f32], k: usize, rescore: usize, exclude: Option<u32>) -> Vec<VectorHit> {
        if self.is_empty() {
            return Vec::new();
        }
        let (qb, _, _) = quantize(query);
        let mut cand: Vec<(u32, usize)> = self
            .bits
            .par_iter()
            .enumerate()
            .filter(|(i, _)| self.keys[*i].0 != u32::MAX && Some(self.keys[*i].0) != exclude)
            .map(|(i, b)| {
                let d: u32 = b.iter().zip(qb.iter()).map(|(x, y)| (x ^ y).count_ones()).sum();
                (d, i)
            })
            .collect();
        let n = rescore.max(k).min(cand.len());
        if n == 0 {
            return Vec::new();
        }
        cand.select_nth_unstable(n - 1);
        cand.truncate(n);
        let mut hits: Vec<VectorHit> = cand
            .into_iter()
            .map(|(_, i)| {
                let s: f32 = self.q8(i).iter().zip(query).map(|(&q, &x)| q as f32 * x).sum::<f32>() * self.scale[i];
                VectorHit { file_id: self.keys[i].0, chunk: self.keys[i].1, score: s }
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(k);
        hits
    }

    /// Like `search`, but one hit per file (its best chunk), up to k files.
    pub fn search_files(&self, query: &[f32], k: usize, exclude: Option<u32>) -> Vec<VectorHit> {
        let hits = self.search(query, k * 4, (k * 8).max(200), exclude);
        let mut seen = std::collections::HashSet::new();
        hits.into_iter().filter(|h| seen.insert(h.file_id)).take(k).collect()
    }

    /// Save to `path` (keys, scales, binary codes) and `path`.q8 (int8 rows), then remap so the
    /// rescoring store is file-backed. Live rows are compacted.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let qpath = path.with_extension("q8");
        let live: Vec<usize> = (0..self.keys.len()).filter(|&i| self.keys[i].0 != u32::MAX).collect();
        {
            let tmp = qpath.with_extension("q8.tmp");
            let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
            for &i in &live {
                let bytes: [u8; DIMS] = self.q8(i).map(|x| x as u8);
                w.write_all(&bytes)?;
            }
            w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            std::fs::rename(tmp, &qpath)?;
        }
        let tmp = path.with_extension("tmp");
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&(DIMS as u32).to_le_bytes())?;
        let m = self.model.as_bytes();
        w.write_all(&(m.len() as u32).to_le_bytes())?;
        w.write_all(m)?;
        w.write_all(&(live.len() as u64).to_le_bytes())?;
        for &i in &live {
            w.write_all(&self.keys[i].0.to_le_bytes())?;
            w.write_all(&self.keys[i].1.to_le_bytes())?;
            w.write_all(&self.scale[i].to_le_bytes())?;
            for x in self.bits[i] {
                w.write_all(&x.to_le_bytes())?;
            }
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        std::fs::rename(tmp, path)?;
        // Swap in the compacted, file-backed layout.
        *self = Self::load(path)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<VectorIndex> {
        let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        if &buf4 != MAGIC {
            return Err(crate::EmbedError::Format("bad vector file".into()));
        }
        r.read_exact(&mut buf4)?;
        if u32::from_le_bytes(buf4) as usize != DIMS {
            return Err(crate::EmbedError::Format("dims changed".into()));
        }
        r.read_exact(&mut buf4)?;
        let mut m = vec![0u8; u32::from_le_bytes(buf4) as usize];
        r.read_exact(&mut m)?;
        let mut idx = VectorIndex::new(&String::from_utf8_lossy(&m));
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8)?;
        let n = u64::from_le_bytes(b8) as usize;
        let qf = std::fs::File::open(path.with_extension("q8"))?;
        // SAFETY: q8 files are replaced via rename, never modified in place.
        let map = unsafe { memmap2::Mmap::map(&qf)? };
        if map.len() != n * DIMS {
            return Err(crate::EmbedError::Format("q8 size mismatch".into()));
        }
        idx.q8_map = Some(std::sync::Arc::new(map));
        idx.keys.reserve(n);
        for i in 0..n {
            r.read_exact(&mut buf4)?;
            let f = u32::from_le_bytes(buf4);
            r.read_exact(&mut buf4)?;
            let c = u32::from_le_bytes(buf4);
            r.read_exact(&mut buf4)?;
            let s = f32::from_le_bytes(buf4);
            let mut bits = [0u64; WORDS];
            for b in bits.iter_mut() {
                r.read_exact(&mut b8)?;
                *b = u64::from_le_bytes(b8);
            }
            idx.keys.push((f, c));
            idx.bits.push(bits);
            idx.q8_src.push(i as u32);
            idx.scale.push(s);
            idx.rows.entry(f).or_default().push(i);
        }
        Ok(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Embedder, HashEmbedder};

    #[test]
    fn finds_nearest_and_persists() {
        let e = HashEmbedder;
        let texts =
            ["rust ownership borrowing lifetimes", "ramen broth tonkotsu noodles", "garden tomatoes watering schedule", "rust borrow checker lifetimes errors"];
        let v = e.embed(&texts);
        let mut idx = VectorIndex::new(e.name());
        for (i, x) in v.iter().enumerate() {
            idx.upsert_file(i as u32, std::slice::from_ref(x));
        }
        let q = e.embed(&["borrow checker lifetimes in rust"]).remove(0);
        let hits = idx.search_files(&q, 2, None);
        let ids: Vec<u32> = hits.iter().map(|h| h.file_id).collect();
        assert!(ids.contains(&3) && ids.contains(&0), "{hits:?}");
        assert!(hits[0].score > 0.3);
        idx.remove_file(3);
        assert!(idx.search_files(&q, 1, None)[0].file_id == 0);
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("v.bin");
        idx.save(&p).unwrap();
        assert_eq!(idx.search_files(&q, 1, None)[0].file_id, 0, "usable after save (remapped)");
        idx.upsert_file(7, std::slice::from_ref(&v[3]));
        assert_eq!(idx.search_files(&q, 1, None)[0].file_id, 7, "owned rows mix with mapped rows");
        idx.remove_file(7);
        let l = VectorIndex::load(&p).unwrap();
        assert_eq!(l.len(), 3);
        assert_eq!(l.search_files(&q, 1, None)[0].file_id, 0);
        assert!(l.file_vector(1).is_some());
    }
}
