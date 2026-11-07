//! Recall: the passages of the vault that best answer a question, each with its source, the
//! reasons it was picked, how the sources link to each other, and what the notes leave out.
//! Feeds the Ask view, `/api/recall` and the MCP `context` tool. Read-only over a snapshot.
//!
//! Candidates are the notes found by BM25 and by chunk vectors, plus one hop of links from the
//! strongest of them. Every chunk of a candidate is scored by meaning (cosine with the query) and
//! by words (local IDF over the candidate chunks); the two are fused as normalised scores together
//! with the note-level rank. The excerpt is the best paragraph run inside the chunk, so a citation
//! points at the lines that matter rather than a whole section.

use crate::query::{NoteRef, note_ref, passes_filters};
use crate::{Engine, Result};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use vault_search::rrf;

#[derive(Debug, Clone)]
pub struct RecallOptions {
    /// Most passages returned.
    pub limit: usize,
    /// Stop adding passages past this many characters of excerpt text (about 4 per token).
    pub budget_chars: usize,
    /// Most passages from one note.
    pub per_note: usize,
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self { limit: 8, budget_chars: 12_000, per_note: 2 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Passage {
    /// Citation number, 1-based, in rank order.
    pub n: u32,
    #[serde(flatten)]
    pub note: NoteRef,
    /// "H1 > H2" path of the section the excerpt is in.
    pub section: Option<String>,
    /// Byte range of the excerpt in the source file, and its first line (1-based).
    pub start: u32,
    pub end: u32,
    pub line: u32,
    pub text: String,
    pub score: f32,
    /// Why it was picked, for people: "matches pricing, churn", "similar meaning", "linked from Home".
    pub why: Vec<String>,
    pub tags: Vec<String>,
    pub mtime_ms: i64,
}

/// A link between two source notes (paths), so a reader can see how the sources connect.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Connection {
    pub from: String,
    pub to: String,
}

/// Something the notes don't settle: no match, a weak match, an old source, a linked note that
/// doesn't exist yet, or two sources that look like near-duplicates.
#[derive(Debug, Clone, Serialize)]
pub struct Gap {
    pub kind: &'static str,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recall {
    pub query: String,
    pub passages: Vec<Passage>,
    pub connections: Vec<Connection>,
    pub gaps: Vec<Gap>,
    pub took_ms: f64,
}

impl Recall {
    /// Plain-text context for a model or an agent: numbered passages with their sources, then the gaps.
    pub fn context_text(&self) -> String {
        let mut out = String::new();
        for p in &self.passages {
            let section = p.section.as_deref().map(|s| format!(" › {s}")).unwrap_or_default();
            out.push_str(&format!("[{}] {}{} (line {}, edited {})\n{}\n\n", p.n, p.note.path, section, p.line, ymd(p.mtime_ms), p.text));
        }
        if !self.connections.is_empty() {
            out.push_str("Links between these notes: ");
            out.push_str(&self.connections.iter().map(|c| format!("{} → {}", c.from, c.to)).collect::<Vec<_>>().join("; "));
            out.push_str("\n\n");
        }
        if !self.gaps.is_empty() {
            out.push_str("Gaps:\n");
            for g in &self.gaps {
                out.push_str(&format!("- {}\n", g.message));
            }
        }
        out
    }
}

/// One scored chunk of a candidate note.
struct Cand {
    fid: u32,
    section: String,
    start: u32,
    end: u32,
    line: u32,
    text: String,
    cos: f32,
    /// Term frequencies of the query terms in the excerpt, aligned with `terms`.
    tf: Vec<u16>,
    tokens: HashSet<String>,
}

/// Cosine at which a chunk counts as close in meaning without sharing a word with the question.
const MEANING: f32 = 0.3;

const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "by", "can", "did", "do", "does", "for", "from", "has", "have", "how", "i", "in", "is", "it", "me",
    "my", "of", "on", "or", "our", "should", "so", "that", "the", "their", "there", "this", "to", "was", "we", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "you", "your",
];

fn words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase())
}

/// Query terms worth matching: lower-cased, deduplicated, without stopwords.
fn query_terms(q: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    words(q).filter(|w| w.chars().count() >= 2 && !STOPWORDS.contains(&w.as_str())).filter(|w| seen.insert(w.clone())).collect()
}

/// A light English stem: one of -ing/-ed/-es/-s/-e off, then a doubled final consonant undone,
/// so "shipping", "ships" and "ship" meet, as do "decided" and "decide".
fn stem(w: &str) -> &str {
    let mut s = w;
    for suf in ["ing", "ed", "es", "s", "e"] {
        if s.len() >= suf.len() + 3 && s.ends_with(suf) {
            s = &s[..s.len() - suf.len()];
            break;
        }
    }
    let b = s.as_bytes();
    if s.len() >= 4 && b[b.len() - 1] == b[b.len() - 2] && !b"aeiouls".contains(&b[b.len() - 1]) {
        s = &s[..s.len() - 1];
    }
    s
}

/// Words match when their stems do.
fn term_matches(term: &str, word: &str) -> bool {
    word == term || stem(term) == stem(word)
}

/// YYYY-MM-DD (UTC) for epoch milliseconds.
pub fn ymd(ms: i64) -> String {
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm), to avoid a date crate.
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The excerpt inside `src[start..end]`: the paragraph with most query-term hits, grown with its
/// neighbours up to about `max` bytes. Returns the byte range, trimmed.
fn excerpt(src: &str, start: usize, end: usize, terms: &[String], max: usize) -> (usize, usize) {
    let body = &src[start..end];
    // Paragraph spans by blank lines.
    let mut paras: Vec<(usize, usize)> = Vec::new();
    let mut s = 0;
    for (i, _) in body.match_indices("\n\n") {
        if i > s {
            paras.push((s, i));
        }
        s = i + 2;
    }
    if s < body.len() {
        paras.push((s, body.len()));
    }
    paras.retain(|&(a, b)| !body[a..b].trim().is_empty());
    if paras.is_empty() {
        return (start, start);
    }
    let hits = |&(a, b): &(usize, usize)| words(&body[a..b]).filter(|w| terms.iter().any(|t| term_matches(t, w))).count();
    let best = (0..paras.len()).max_by_key(|&i| (hits(&paras[i]), std::cmp::Reverse(i))).unwrap_or(0);
    let (mut lo, mut hi) = (best, best);
    // Grow toward whichever neighbour has more hits while the run still fits.
    loop {
        let down = (hi + 1 < paras.len() && paras[hi + 1].1 - paras[lo].0 <= max).then(|| hits(&paras[hi + 1]));
        let up = (lo > 0 && paras[hi].1 - paras[lo - 1].0 <= max).then(|| hits(&paras[lo - 1]));
        match (down, up) {
            (Some(d), Some(u)) if u > d => lo -= 1,
            (Some(_), _) => hi += 1,
            (None, Some(_)) => lo -= 1,
            (None, None) => break,
        }
    }
    let (mut a, mut b) = (start + paras[lo].0, start + paras[hi].1);
    // A single paragraph longer than `max` is cut at a char boundary.
    if b - a > max * 2 {
        b = (a + max * 2..=b).find(|i| src.is_char_boundary(*i)).unwrap_or(b);
    }
    while a < b && src.as_bytes()[a].is_ascii_whitespace() {
        a += 1;
    }
    // The section is shown with the passage, so a leading heading line is dropped when text follows it.
    if src[a..b].starts_with('#')
        && let Some(nl) = src[a..b].find('\n')
        && !src[a + nl..b].trim().is_empty()
    {
        a += nl;
        while a < b && src.as_bytes()[a].is_ascii_whitespace() {
            a += 1;
        }
    }
    while b > a && src.as_bytes()[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    (a, b)
}

impl Engine {
    /// The passages that best answer `q`, with sources, reasons, links between the sources, and gaps.
    /// Accepts the search syntax (`tag:x`, `path:dir`, `-word`, "phrases") to scope the recall.
    pub fn recall(&self, q: &str, opts: &RecallOptions) -> Result<Recall> {
        let t0 = Instant::now();
        let snap = self.snapshot();
        let pq = vault_search::parse_query(q);
        let free = pq.text();
        let terms = query_terms(&free);
        let phrase = free.to_lowercase();
        let emb = self.0.embedder.read().clone();
        let (text, (qv, vhits)) = rayon::join(
            || self.0.text.search(q, 40, false),
            || {
                if free.trim().is_empty() {
                    return (Vec::new(), Vec::new());
                }
                let qv = emb.embed(&[free.as_str()]).pop().unwrap_or_default();
                let hits = self.0.vectors.read().search(&qv, 150, 600, None);
                (qv, hits)
            },
        );
        let text = text?;
        let ok = |id: u32| snap.get(id).is_some_and(|e| e.markdown && passes_filters(e, &pq));

        // Note-level ranking: BM25 notes and the notes of the best chunks.
        let t_ids: Vec<u32> = text.iter().map(|h| h.file_id).filter(|&id| ok(id)).collect();
        let mut v_ids: Vec<u32> = Vec::new();
        for h in vhits.iter().filter(|h| h.score > 0.2) {
            if !v_ids.contains(&h.file_id) && ok(h.file_id) {
                v_ids.push(h.file_id);
            }
        }
        let fused = rrf(&[&t_ids, &v_ids]);
        let mut prior: HashMap<u32, f32> = fused.iter().take(24).copied().collect();
        let mut via: HashMap<u32, String> = HashMap::new();
        // One hop of links from the three strongest notes: answers often sit next door.
        for &(seed, s) in fused.iter().take(3) {
            let Some(e) = snap.get(seed) else { continue };
            let out = e.links.iter().filter_map(|l| l.dst()).map(|d| (d, format!("linked from {}", e.title)));
            let back = snap.backlinks(seed).iter().map(|&b| (b, format!("links to {}", e.title)));
            for (n, why) in out.chain(back).take(16) {
                if n != seed && ok(n) && !prior.contains_key(&n) {
                    prior.insert(n, s * 0.6);
                    via.insert(n, why);
                }
            }
        }

        // Score every chunk of every candidate.
        let files: Vec<u32> = prior.keys().copied().collect();
        let vectors = self.0.vectors.read();
        let cands: Vec<Cand> = files
            .par_iter()
            .flat_map_iter(|&fid| {
                let mut out = Vec::new();
                let Some(e) = snap.get(fid) else { return out };
                let Some(src) = self.read_src(&e.path) else { return out };
                let parsed = vault_parse::parse(&e.path, &src);
                let chunks = vault_parse::chunk_note(&src, &parsed);
                if chunks.is_empty() || qv.is_empty() && terms.is_empty() {
                    return out;
                }
                let vecs = match vectors.file_vector_rows(fid) {
                    Some(rows) if rows.len() == chunks.len() => rows,
                    _ => emb.embed(&chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>()),
                };
                for (c, v) in chunks.iter().zip(vecs.iter()) {
                    let cos = if qv.is_empty() { 0.0 } else { vault_embed::dot(&qv, v) };
                    let (a, b) = excerpt(&src, (c.start as usize).min(src.len()), (c.end as usize).min(src.len()), &terms, 900);
                    if b <= a {
                        continue;
                    }
                    let ex = &src[a..b];
                    let toks: Vec<String> = words(ex).collect();
                    let tf = terms.iter().map(|t| toks.iter().filter(|w| term_matches(t, w)).count().min(u16::MAX as usize) as u16).collect();
                    out.push(Cand {
                        fid,
                        section: c.heading_path.clone(),
                        start: a as u32,
                        end: b as u32,
                        line: src[..a].matches('\n').count() as u32 + 1,
                        text: ex.to_string(),
                        cos,
                        tf,
                        tokens: toks.into_iter().collect(),
                    });
                }
                out
            })
            .collect();
        drop(vectors);

        // Words: BM25-style saturation with IDF over the candidate chunks; an exact phrase counts extra.
        let n = cands.len().max(1) as f32;
        let idf: Vec<f32> = (0..terms.len())
            .map(|i| {
                let df = cands.iter().filter(|c| c.tf[i] > 0).count() as f32;
                (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
            })
            .collect();
        let lex = |c: &Cand| -> f32 {
            let mut s: f32 = c.tf.iter().zip(&idf).map(|(&tf, w)| w * tf as f32 / (tf as f32 + 1.2)).sum();
            if terms.len() > 1 && c.text.to_lowercase().contains(&phrase) {
                s += 1.0;
            }
            let title = snap.get(c.fid).map(|e| e.title.to_lowercase()).unwrap_or_default();
            s + terms.iter().zip(&idf).filter(|(t, _)| words(&title).any(|w| term_matches(t, &w))).map(|(_, w)| 0.5 * w).sum::<f32>()
        };
        let lex_scores: Vec<f32> = cands.iter().map(lex).collect();
        // Fuse by normalised score, not rank: within one small candidate set, rank fusion flattens
        // a clear winner into a near-tie that note popularity then decides.
        let max_lex = lex_scores.iter().copied().fold(0f32, f32::max).max(1e-6);
        let max_cos = cands.iter().map(|c| c.cos).fold(0f32, f32::max).max(1e-6);
        let max_prior = prior.values().copied().fold(0f32, f32::max).max(1e-6);
        let now = crate::now_ms();
        let score: Vec<f32> = cands
            .iter()
            .zip(&lex_scores)
            .map(|(c, &l)| {
                let p = prior.get(&c.fid).copied().unwrap_or(0.0) / max_prior;
                let base = 0.45 * (c.cos.max(0.0) / max_cos) + 0.4 * (l / max_lex) + 0.15 * p;
                base * (1.0 + 0.05 * snap.pagerank_norm(c.fid) + 0.05 * snap.frecency_norm(c.fid, now))
            })
            .collect();
        // A passage earns its place with a query word or a clearly similar meaning.
        let mut order: Vec<usize> = (0..cands.len()).filter(|&i| cands[i].cos >= MEANING || lex_scores[i] > 0.0).collect();
        order.sort_by(|&a, &b| score[b].total_cmp(&score[a]));

        // Pick: per-note cap, skip near-identical excerpts, stop at the limit or the budget.
        let mut picked: Vec<usize> = Vec::new();
        let mut per: HashMap<u32, usize> = HashMap::new();
        let mut used = 0usize;
        for i in order {
            let c = &cands[i];
            if picked.len() >= opts.limit || (used > 0 && used + c.text.len() > opts.budget_chars) {
                break;
            }
            if per.get(&c.fid).copied().unwrap_or(0) >= opts.per_note {
                continue;
            }
            let dup = picked.iter().any(|&j| {
                let o = &cands[j];
                let inter = c.tokens.intersection(&o.tokens).count() as f32;
                let union = c.tokens.union(&o.tokens).count().max(1) as f32;
                inter / union > 0.7 || (o.fid == c.fid && c.start < o.end && o.start < c.end)
            });
            if dup {
                continue;
            }
            used += c.text.len();
            *per.entry(c.fid).or_default() += 1;
            picked.push(i);
        }

        let mut passages = Vec::new();
        for (k, &i) in picked.iter().enumerate() {
            let c = &cands[i];
            let Some(e) = snap.get(c.fid) else { continue };
            let mut why = Vec::new();
            let matched: Vec<&str> = terms.iter().zip(&c.tf).filter(|(_, tf)| **tf > 0).map(|(t, _)| t.as_str()).take(4).collect();
            if !matched.is_empty() {
                why.push(format!("matches {}", matched.join(", ")));
            }
            if c.cos >= MEANING {
                why.push("similar meaning".to_string());
            }
            if let Some(v) = via.get(&c.fid) {
                why.push(v.clone());
            }
            passages.push(Passage {
                n: k as u32 + 1,
                note: note_ref(e),
                section: (!c.section.is_empty()).then(|| c.section.clone()),
                start: c.start,
                end: c.end,
                line: c.line,
                text: c.text.clone(),
                score: score[i],
                why,
                tags: e.tags.clone(),
                mtime_ms: e.mtime_ns / 1_000_000,
            });
        }

        // How the sources link to each other.
        let src_ids: Vec<u32> = {
            let mut v: Vec<u32> = Vec::new();
            for p in &passages {
                if !v.contains(&p.note.id) {
                    v.push(p.note.id);
                }
            }
            v
        };
        let mut connections = Vec::new();
        for &a in &src_ids {
            let Some(ea) = snap.get(a) else { continue };
            for &b in &src_ids {
                if a != b && ea.links.iter().any(|l| l.dst() == Some(b)) {
                    let c = Connection { from: ea.path.clone(), to: snap.get(b).map(|e| e.path.clone()).unwrap_or_default() };
                    if !connections.contains(&c) {
                        connections.push(c);
                    }
                }
            }
        }

        let gaps = self.gaps(&snap, &passages, &cands, &picked, &terms, &src_ids, now);
        Ok(Recall { query: q.to_string(), passages, connections, gaps, took_ms: t0.elapsed().as_secs_f64() * 1000.0 })
    }

    #[allow(clippy::too_many_arguments)]
    fn gaps(&self, snap: &crate::Snapshot, passages: &[Passage], cands: &[Cand], picked: &[usize], terms: &[String], src_ids: &[u32], now: i64) -> Vec<Gap> {
        let mut gaps = Vec::new();
        if passages.is_empty() {
            gaps.push(Gap { kind: "nothing", message: "Nothing in your notes matches this.".into(), path: None });
            return gaps;
        }
        let best_cos = picked.iter().map(|&i| cands[i].cos).fold(0f32, f32::max);
        let any_words = picked.iter().any(|&i| cands[i].tf.iter().any(|&t| t > 0));
        // Only meaning matched, and not strongly.
        if !any_words && best_cos < 0.45 {
            gaps.push(Gap { kind: "weak", message: "These notes only loosely match; your notes may not cover this.".into(), path: None });
        }
        // Linked from a source but never written, when the link name touches the question.
        for &id in src_ids {
            let Some(e) = snap.get(id) else { continue };
            for l in e.links.iter().filter(|l| l.dst().is_none() && !l.embed()) {
                let name = snap.sym(l.target).to_string();
                if words(&name).any(|w| terms.iter().any(|t| term_matches(t, &w) || term_matches(&w, t))) {
                    let message = format!("{} links to [[{}]], which doesn't exist yet.", e.title, name);
                    if !gaps.iter().any(|g: &Gap| g.message == message) {
                        gaps.push(Gap { kind: "missing_note", message, path: Some(e.path.clone()) });
                    }
                }
            }
        }
        // Old sources: the answer may have moved on.
        let year_ms = 365 * 86_400_000i64;
        for &id in src_ids.iter().take(5) {
            let Some(e) = snap.get(id) else { continue };
            let age = now - e.mtime_ns / 1_000_000;
            if age > year_ms {
                let months = age / (30 * 86_400_000);
                gaps.push(Gap { kind: "stale", message: format!("{} was last edited {} months ago.", e.title, months), path: Some(e.path.clone()) });
            }
        }
        // Near-duplicate sources may disagree with each other.
        for (k, &a) in src_ids.iter().enumerate() {
            let Some(ea) = snap.get(a) else { continue };
            for (other, sim) in self.near_duplicates(&ea.path) {
                if src_ids[k + 1..].contains(&other.id) && sim > 0.5 {
                    gaps.push(Gap {
                        kind: "duplicate",
                        message: format!("{} and {} look like near-duplicates; check they agree.", ea.title, other.title),
                        path: Some(ea.path.clone()),
                    });
                }
            }
        }
        gaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_stopwords_and_match_inflections() {
        assert_eq!(query_terms("What did we decide about the Beta launch?"), vec!["decide", "beta", "launch"]);
        assert!(term_matches("decide", "decided"));
        assert!(!term_matches("plan", "planetarium"));
        assert!(term_matches("ship", "ship"));
        assert!(term_matches("shipping", "ship"));
        assert!(term_matches("notes", "note"));
        assert!(!term_matches("ship", "shop"));
    }

    #[test]
    fn excerpt_picks_the_matching_paragraph() {
        let src = "# T\n\nIntro words here.\n\nWe decided to ship the beta.\n\nUnrelated closing text.";
        let (a, b) = excerpt(src, 0, src.len(), &["ship".into()], 30);
        assert_eq!(&src[a..b], "We decided to ship the beta.");
        let (a, b) = excerpt(src, 0, src.len(), &["ship".into()], 900);
        assert!(src[a..b].starts_with("Intro") && src[a..b].contains("closing"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_793_989_431_000), "2026-11-06");
    }
}
