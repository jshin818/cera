//! Read-side API: switcher, search, backlinks, mentions, related notes, suggestions, clusters,
//! tags, folders and graph data. All of these read an immutable snapshot.

use crate::{Engine, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use vault_search::rrf;

#[derive(Debug, Clone, Serialize)]
pub struct NoteRef {
    pub id: u32,
    pub path: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwitchResult {
    #[serde(flatten)]
    pub note: NoteRef,
    pub alias: Option<String>,
    pub typo: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    #[serde(flatten)]
    pub note: NoteRef,
    pub score: f32,
    /// HTML (escaped text with <b> highlights) from the BM25 side; empty for vector-only hits.
    pub snippet: String,
    /// Heading path of the best-matching chunk (vector side), if any.
    pub section: Option<String>,
    pub text_rank: Option<u32>,
    pub vector_rank: Option<u32>,
    pub tags: Vec<String>,
    pub mtime_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    #[serde(flatten)]
    pub note: NoteRef,
    /// Source offsets of the occurrence and a one-line excerpt around it.
    pub start: u32,
    pub end: u32,
    pub line: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Related {
    #[serde(flatten)]
    pub note: NoteRef,
    pub score: f32,
    /// Why: "similar", "linked nearby", or both.
    pub why: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    #[serde(flatten)]
    pub note: NoteRef,
    pub score: f32,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cluster {
    pub id: u32,
    pub size: usize,
    pub label: String,
    pub top: Vec<NoteRef>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirListing {
    pub path: String,
    pub dirs: Vec<DirInfo>,
    pub notes: Vec<NoteItem>,
    pub total_notes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirInfo {
    pub path: String,
    pub name: String,
    pub notes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteItem {
    pub path: String,
    pub title: String,
    pub mtime_ms: i64,
    pub markdown: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphNode {
    pub id: u32,
    pub title: String,
    pub path: String,
    pub x: f32,
    pub y: f32,
    pub rank: f32,
    pub cluster: u32,
    pub degree: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(u32, u32)>,
    pub center: Option<u32>,
    pub clusters: usize,
    /// When true, nodes are cluster aggregates (level of detail).
    pub aggregated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VaultStats {
    pub notes: usize,
    pub attachments: usize,
    pub folders: usize,
    pub words: u64,
    pub links: usize,
    pub unresolved: usize,
    pub tags: usize,
    pub orphans: usize,
    pub tasks_open: u64,
    pub tasks_done: u64,
    /// Notes modified per day over the last 26 weeks (oldest first).
    pub activity: Vec<u32>,
}

fn note_ref(e: &crate::Entry) -> NoteRef {
    NoteRef { id: e.id, path: e.path.clone(), title: e.title.clone() }
}

fn line_around(src: &str, start: usize, end: usize) -> String {
    let start = start.min(src.len());
    let end = end.min(src.len()).max(start);
    let ls = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let le = src[end..].find('\n').map(|i| end + i).unwrap_or(src.len());
    let line = src[ls..le].trim();
    if line.chars().count() > 220 {
        // Center the excerpt on the occurrence.
        let rel = start - ls;
        let from = src[ls..ls + rel].char_indices().rev().nth(90).map(|(i, _)| i).unwrap_or(0);
        let s: String = src[ls + from..le].chars().take(200).collect();
        format!("…{}…", s.trim())
    } else {
        line.to_string()
    }
}

impl Engine {
    fn read_src(&self, path: &str) -> Option<String> {
        vault_fs::read(&self.0.root, path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Resolve a link target written in `from` to a vault path (Obsidian rules).
    pub fn resolve(&self, target: &str, from: &str) -> Option<String> {
        let st = self.0.state.lock();
        match st.resolver.resolve(target, from, u32::MAX) {
            vault_parse::Resolved::File(id) => st.entries.get(id as usize).and_then(|e| e.as_ref()).map(|e| e.path.clone()),
            vault_parse::Resolved::Unresolved => None,
        }
    }

    /// Link targets that resolve to no file, with the notes that use them (most used first).
    pub fn unresolved(&self) -> Vec<(String, Vec<NoteRef>)> {
        let snap = self.snapshot();
        let st = self.0.state.lock();
        let mut v: Vec<(String, Vec<NoteRef>)> = st.unresolved.iter().map(|(t, srcs)| (t.clone(), srcs.iter().filter_map(|&s| snap.get(s).map(|e| note_ref(e))).collect())).collect();
        v.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        v
    }

    /// Quick switcher (Section 6.2).
    pub fn switch(&self, q: &str, limit: usize) -> Vec<SwitchResult> {
        let Some(sw) = self.0.switcher.load_full() else { return Vec::new() };
        let snap = self.snapshot();
        sw.query(q, limit)
            .into_iter()
            .filter_map(|h| snap.get(h.id).map(|e| SwitchResult { note: note_ref(e), alias: h.alias, typo: h.typo }))
            .collect()
    }

    /// Full-text only (BM25F).
    pub fn search_text(&self, q: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let snap = self.snapshot();
        let hits = self.0.text.search(q, limit, true)?;
        Ok(hits
            .into_iter()
            .enumerate()
            .filter_map(|(i, h)| {
                snap.get(h.file_id).map(|e| SearchResult {
                    note: note_ref(e),
                    score: h.score,
                    snippet: h.snippet,
                    section: None,
                    text_rank: Some(i as u32),
                    vector_rank: None,
                    tags: e.tags.clone(),
                    mtime_ms: e.mtime_ns / 1_000_000,
                })
            })
            .collect())
    }

    /// Hybrid search (Section 6.4): BM25 top-100 and vector top-100 in parallel, fused with RRF,
    /// then boosted by frecency and PageRank.
    pub fn search(&self, q: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let snap = self.snapshot();
        let pq = vault_search::parse_query(q);
        let free_text = pq.text();
        let has_filters = pq.must.iter().any(|c| matches!(c, vault_search::Clause::Tag(_) | vault_search::Clause::Path(_))) || !pq.exclude.is_empty();
        let (text, vec) = rayon::join(
            || self.0.text.search(q, 100, true),
            || {
                if free_text.split_whitespace().count() == 0 {
                    return Vec::new();
                }
                let emb = self.0.embedder.read().clone();
                let qv = emb.embed(&[free_text.as_str()]).pop().unwrap_or_default();
                self.0.vectors.read().search_files(&qv, 100, None)
            },
        );
        let text = text?;
        let mut vec = vec;
        // Vector hits must satisfy tag/path/exclusion filters too; check them against the text side's
        // filter semantics cheaply via the snapshot.
        if has_filters {
            vec.retain(|h| snap.get(h.file_id).is_some_and(|e| passes_filters(e, &pq)));
        }
        // Drop weak vector-only matches so unrelated notes don't pad the list.
        let vec: Vec<_> = vec.into_iter().filter(|h| h.score > 0.15).collect();
        let t_ids: Vec<u32> = text.iter().map(|h| h.file_id).collect();
        let v_ids: Vec<u32> = vec.iter().map(|h| h.file_id).collect();
        let fused = rrf(&[&t_ids, &v_ids]);
        let now = crate::now_ms();
        let snippets: HashMap<u32, &str> = text.iter().map(|h| (h.file_id, h.snippet.as_str())).collect();
        let sections: HashMap<u32, u32> = vec.iter().map(|h| (h.file_id, h.chunk)).collect();
        let mut out: Vec<SearchResult> = fused
            .into_iter()
            .filter_map(|(id, s)| {
                let e = snap.get(id)?;
                let score = s * (1.0 + 0.3 * snap.frecency_norm(id, now) + 0.2 * snap.pagerank_norm(id));
                let section = sections.get(&id).and_then(|&c| self.0.store.chunks_of(id).ok().and_then(|cs| cs.get(c as usize).map(|x| x.heading_path.clone()))).filter(|s| !s.is_empty());
                Some(SearchResult {
                    note: note_ref(e),
                    score,
                    snippet: snippets.get(&id).map(|s| s.to_string()).unwrap_or_default(),
                    section,
                    text_rank: t_ids.iter().position(|x| *x == id).map(|p| p as u32),
                    vector_rank: v_ids.iter().position(|x| *x == id).map(|p| p as u32),
                    tags: e.tags.clone(),
                    mtime_ms: e.mtime_ns / 1_000_000,
                })
            })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        out.truncate(limit);
        // Fill snippets for vector-only hits from their best chunk.
        for r in out.iter_mut().filter(|r| r.snippet.is_empty()) {
            if let (Some(&c), Some(src)) = (sections.get(&r.note.id), self.read_src(&r.note.path)) {
                if let Some(ch) = self.0.store.chunks_of(r.note.id).ok().and_then(|cs| cs.get(c as usize).cloned()) {
                    let s = (ch.start as usize).min(src.len());
                    let e = (ch.end as usize).min(src.len());
                    let s = (s..=e).find(|i| src.is_char_boundary(*i)).unwrap_or(e);
                    let txt: String = src[s..e].split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
                    r.snippet = html_escape(&txt);
                }
            }
        }
        Ok(out)
    }

    /// Notes linking to `path`, each with the line around each link.
    pub fn backlinks(&self, path: &str) -> Result<Vec<Context>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?;
        let mut out = Vec::new();
        for &src_id in &e.backlinks {
            let Some(src) = snap.get(src_id) else { continue };
            let Some(text) = self.read_src(&src.path) else { continue };
            for l in src.links.iter().filter(|l| l.dst == Some(e.id)) {
                out.push(Context { note: note_ref(src), start: l.start, end: l.end, line: line_around(&text, l.start as usize, l.end as usize) });
            }
        }
        Ok(out)
    }

    /// Other notes that mention this note's title or aliases without linking it. Candidates come
    /// from a phrase query; each is verified on the text with word-boundary checks.
    pub fn unlinked_mentions(&self, path: &str, limit: usize) -> Result<Vec<Context>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?;
        let names: Vec<String> = std::iter::once(e.title.clone()).chain(e.aliases.iter().cloned()).filter(|n| n.chars().count() >= 3).collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let idx = vault_graph::MentionIndex::new(names.iter().map(|n| (e.id, n.clone())), 3);
        let mut cands: Vec<u32> = Vec::new();
        for n in &names {
            let q = format!("\"{}\"", n.replace('"', " "));
            for h in self.0.text.search(&q, 200, false)? {
                if h.file_id != e.id && !cands.contains(&h.file_id) {
                    cands.push(h.file_id);
                }
            }
        }
        let mut out = Vec::new();
        for c in cands {
            let Some(src) = snap.get(c) else { continue };
            let Some(text) = self.read_src(&src.path) else { continue };
            let mut skip: Vec<(u32, u32)> = src.links.iter().map(|l| (l.start, l.end)).collect();
            skip.push((0, src_frontmatter_end(&text)));
            for m in idx.find(&text, u32::MAX, &skip) {
                out.push(Context { note: note_ref(src), start: m.start, end: m.end, line: line_around(&text, m.start as usize, m.end as usize) });
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    /// Titles of other notes mentioned in this note but not linked (Aho-Corasick over all titles).
    pub fn outgoing_mentions(&self, path: &str) -> Result<Vec<Context>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?;
        let Some(idx) = self.0.mentions.load_full() else { return Ok(Vec::new()) };
        let text = self.read_src(path).unwrap_or_default();
        let mut skip: Vec<(u32, u32)> = e.links.iter().map(|l| (l.start, l.end)).collect();
        skip.push((0, src_frontmatter_end(&text)));
        let linked: HashSet<u32> = e.links.iter().filter_map(|l| l.dst).collect();
        let mut seen = HashSet::new();
        Ok(idx
            .find(&text, e.id, &skip)
            .into_iter()
            .filter(|m| !linked.contains(&m.target) && seen.insert(m.target))
            .filter_map(|m| snap.get(m.target).map(|t| Context { note: note_ref(t), start: m.start, end: m.end, line: line_around(&text, m.start as usize, m.end as usize) }))
            .collect())
    }

    /// Related notes (Section 7): RRF of embedding kNN of the note's mean vector and personalized
    /// PageRank from the note. Cached per note until the note, the graph or the vectors change.
    pub fn related(&self, path: &str, k: usize) -> Result<Vec<Related>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?.clone();
        let vgen = self.0.vector_gen.load(Ordering::SeqCst);
        if let Some((g, vg, r)) = self.0.related_cache.lock().get(&e.id) {
            if *g == snap.generation && *vg == vgen {
                return Ok(r.iter().take(k).cloned().collect());
            }
        }
        let (vec_ids, ppr_ids) = rayon::join(
            || {
                let v = self.0.vectors.read();
                match v.file_vector(e.id) {
                    Some(q) => v.search_files(&q, 50, Some(e.id)).into_iter().filter(|h| h.score > 0.2).map(|h| h.file_id).collect::<Vec<u32>>(),
                    None => Vec::new(),
                }
            },
            || {
                let g = self.local_graph(&snap, e.id, 2, 2000);
                let p = vault_graph::personalized_pagerank(&g.0, &[g.1[&e.id]], 0.15, 1e-4);
                let mut v: Vec<(u32, f32)> = p.into_iter().filter(|(n, _)| *n != g.1[&e.id]).map(|(n, s)| (g.2[n as usize], s)).collect();
                v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                v.into_iter().take(50).map(|x| x.0).collect::<Vec<u32>>()
            },
        );
        let fused = rrf(&[&vec_ids, &ppr_ids]);
        let out: Vec<Related> = fused
            .into_iter()
            .filter(|(id, _)| *id != e.id)
            .filter_map(|(id, s)| {
                let n = snap.get(id).filter(|n| n.markdown)?;
                let mut why = Vec::new();
                if vec_ids.contains(&id) {
                    why.push("similar");
                }
                if ppr_ids.contains(&id) {
                    why.push("linked nearby");
                }
                Some(Related { note: note_ref(n), score: s, why })
            })
            .take(30)
            .collect();
        self.0.related_cache.lock().insert(e.id, (snap.generation, vgen, out.clone()));
        Ok(out.into_iter().take(k).collect())
    }

    /// BFS neighbourhood as a compact graph: (graph, file id -> local id, local id -> file id).
    fn local_graph(&self, snap: &crate::Snapshot, center: u32, depth: usize, max_nodes: usize) -> (vault_graph::Graph, HashMap<u32, u32>, Vec<u32>) {
        let mut ids: Vec<u32> = vec![center];
        let mut local: HashMap<u32, u32> = HashMap::from([(center, 0)]);
        let mut frontier = vec![center];
        let mut edges: Vec<(u32, u32)> = Vec::new();
        for _ in 0..depth {
            let mut next = Vec::new();
            for &f in &frontier {
                let Some(e) = snap.get(f) else { continue };
                let nbrs = e.links.iter().filter_map(|l| l.dst).chain(e.backlinks.iter().copied());
                for n in nbrs {
                    if snap.get(n).is_none_or(|x| !x.markdown) || n == f {
                        continue;
                    }
                    if !local.contains_key(&n) {
                        if ids.len() >= max_nodes {
                            continue;
                        }
                        local.insert(n, ids.len() as u32);
                        ids.push(n);
                        next.push(n);
                    }
                    edges.push((local[&f], local[&n]));
                }
            }
            frontier = next;
        }
        (vault_graph::Graph::new(ids.len(), &edges), local, ids)
    }

    /// Link suggestions: Adamic-Adar on the link graph fused (RRF) with embedding cosine.
    pub fn link_suggestions(&self, path: &str, k: usize) -> Result<Vec<Suggestion>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?.clone();
        let linked: HashSet<u32> = e.links.iter().filter_map(|l| l.dst).collect();
        let (g, local, ids) = self.local_graph(&snap, e.id, 2, 5000);
        let aa: Vec<u32> = vault_graph::adamic_adar(&g, local[&e.id], 50).into_iter().map(|(n, _)| ids[n as usize]).filter(|id| !linked.contains(id)).collect();
        let cos: Vec<u32> = {
            let v = self.0.vectors.read();
            v.file_vector(e.id).map(|q| v.search_files(&q, 50, Some(e.id)).into_iter().filter(|h| h.score > 0.25).map(|h| h.file_id).filter(|id| !linked.contains(id)).collect()).unwrap_or_default()
        };
        let mentioned: HashSet<u32> = self.outgoing_mentions(path)?.into_iter().map(|c| c.note.id).collect();
        let m_list: Vec<u32> = mentioned.iter().copied().collect();
        Ok(rrf(&[&aa, &cos, &m_list])
            .into_iter()
            .filter(|(id, _)| *id != e.id && !linked.contains(id))
            .filter_map(|(id, s)| {
                let n = snap.get(id).filter(|n| n.markdown)?;
                let reason = if mentioned.contains(&id) {
                    "mentioned here"
                } else if aa.contains(&id) && cos.contains(&id) {
                    "shared links and similar"
                } else if aa.contains(&id) {
                    "shares links"
                } else {
                    "similar content"
                };
                Some(Suggestion { note: note_ref(n), score: s, reason })
            })
            .take(k)
            .collect())
    }

    /// Tag suggestions: cosine between the note vector and each existing tag's centroid. Never auto-applied.
    pub fn tag_suggestions(&self, path: &str, k: usize) -> Result<Vec<(String, f32)>> {
        let snap = self.snapshot();
        let e = snap.by_path(path).ok_or_else(|| crate::CoreError::NotFound(path.into()))?.clone();
        let vgen = self.0.vector_gen.load(Ordering::SeqCst);
        let v = self.0.vectors.read();
        let Some(q) = v.file_vector(e.id) else { return Ok(Vec::new()) };
        let mut cache = self.0.tag_centroids.lock();
        if cache.as_ref().is_none_or(|(g, _)| *g != vgen ^ snap.generation) {
            let mut acc: HashMap<&str, (Vec<f32>, usize)> = HashMap::new();
            for n in snap.notes() {
                if n.tags.is_empty() {
                    continue;
                }
                if let Some(fv) = v.file_vector(n.id) {
                    for t in &n.tags {
                        let a = acc.entry(t.as_str()).or_insert_with(|| (vec![0.0; fv.len()], 0));
                        a.0.iter_mut().zip(&fv).for_each(|(x, y)| *x += y);
                        a.1 += 1;
                    }
                }
            }
            let cents = acc
                .into_iter()
                .filter(|(_, (_, c))| *c >= 2)
                .map(|(t, (mut c, _))| {
                    vault_embed::normalize(&mut c);
                    (t.to_string(), c)
                })
                .collect();
            *cache = Some((vgen ^ snap.generation, cents));
        }
        let mut out: Vec<(String, f32)> = cache.as_ref().unwrap().1.iter().filter(|(t, _)| !e.tags.contains(t)).map(|(t, c)| (t.clone(), vault_embed::dot(&q, c))).filter(|(_, s)| *s > 0.3).collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(k);
        Ok(out)
    }

    /// Near-duplicates of a note (MinHash, Jaccard >= 0.8).
    pub fn near_duplicates(&self, path: &str) -> Vec<(NoteRef, f32)> {
        let snap = self.snapshot();
        let Some(e) = snap.by_path(path) else { return Vec::new() };
        self.0.neardup.lock().near(e.id, 0.8).into_iter().filter_map(|(id, j)| snap.get(id).map(|n| (note_ref(n), j))).collect()
    }

    pub fn all_near_duplicates(&self) -> Vec<(NoteRef, NoteRef, f32)> {
        let snap = self.snapshot();
        self.0.neardup.lock().all_pairs(0.8).into_iter().filter_map(|(a, b, j)| Some((note_ref(snap.get(a)?), note_ref(snap.get(b)?), j))).collect()
    }

    pub fn tags(&self) -> Vec<TagCount> {
        let snap = self.snapshot();
        let mut m: HashMap<String, usize> = HashMap::new();
        for e in snap.notes() {
            let mut seen = HashSet::new();
            for t in &e.tags {
                let mut acc = String::new();
                for part in t.split('/') {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    if seen.insert(acc.clone()) {
                        *m.entry(acc.clone()).or_default() += 1;
                    }
                }
            }
        }
        let mut v: Vec<TagCount> = m.into_iter().map(|(tag, count)| TagCount { tag, count }).collect();
        v.sort_by(|a, b| b.count.cmp(&a.count).then(a.tag.cmp(&b.tag)));
        v
    }

    pub fn notes_with_tag(&self, tag: &str) -> Vec<NoteRef> {
        let snap = self.snapshot();
        let t = tag.trim_start_matches('#').to_lowercase();
        let prefix = format!("{t}/");
        let mut v: Vec<&Arc<crate::Entry>> = snap.notes().filter(|e| e.tags.iter().any(|x| *x == t || x.starts_with(&prefix))).collect();
        v.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns));
        v.into_iter().map(|e| note_ref(e)).collect()
    }

    /// Leiden clusters, labelled by their most common tags (or else their top note).
    pub fn clusters(&self, min_size: usize) -> Vec<Cluster> {
        let snap = self.snapshot();
        let mut members: HashMap<u32, Vec<&Arc<crate::Entry>>> = HashMap::new();
        for e in snap.notes() {
            if let Some(c) = snap.cluster(e.id) {
                members.entry(c).or_default().push(e);
            }
        }
        let mut out: Vec<Cluster> = members
            .into_iter()
            .filter(|(_, m)| m.len() >= min_size)
            .map(|(id, mut m)| {
                m.sort_by(|a, b| snap.pagerank_norm(b.id).total_cmp(&snap.pagerank_norm(a.id)));
                let mut tc: HashMap<&str, usize> = HashMap::new();
                for e in &m {
                    for t in &e.tags {
                        *tc.entry(t.as_str()).or_default() += 1;
                    }
                }
                let mut tags: Vec<(&str, usize)> = tc.into_iter().filter(|(_, c)| *c * 4 >= m.len()).collect();
                tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let tags: Vec<String> = tags.into_iter().take(3).map(|(t, _)| t.to_string()).collect();
                let label = m.first().map(|e| e.title.clone()).unwrap_or_default();
                Cluster { id, size: m.len(), label, top: m.iter().take(8).map(|e| note_ref(e)).collect(), tags }
            })
            .collect();
        out.sort_by(|a, b| b.size.cmp(&a.size).then(a.id.cmp(&b.id)));
        out
    }

    /// One folder level for the sidebar (folders are loaded lazily, so this stays small at 100k).
    pub fn list_dir(&self, dir: &str) -> DirListing {
        let snap = self.snapshot();
        let dir = dir.trim_matches('/');
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut notes = Vec::new();
        let mut total = 0;
        for e in snap.iter() {
            let Some(rest) = e.path.strip_prefix(&prefix) else { continue };
            if e.markdown {
                total += 1;
            }
            match rest.split_once('/') {
                Some((child, _)) => {
                    if e.markdown {
                        *counts.entry(child).or_default() += 1;
                    }
                }
                None => notes.push(NoteItem { path: e.path.clone(), title: e.title.clone(), mtime_ms: e.mtime_ns / 1_000_000, markdown: e.markdown }),
            }
        }
        let mut dirs: Vec<DirInfo> = snap
            .dirs
            .iter()
            .filter_map(|d| {
                let rest = d.strip_prefix(&prefix)?;
                (!rest.is_empty() && !rest.contains('/')).then(|| DirInfo { path: d.clone(), name: rest.to_string(), notes: counts.get(rest).copied().unwrap_or(0) })
            })
            .collect();
        dirs.sort_by_key(|d| d.name.to_lowercase());
        notes.sort_by(|a, b| b.markdown.cmp(&a.markdown).then(a.title.to_lowercase().cmp(&b.title.to_lowercase())));
        DirListing { path: dir.to_string(), dirs, notes, total_notes: total }
    }

    /// Most recently modified notes.
    pub fn recent(&self, limit: usize) -> Vec<NoteItem> {
        let snap = self.snapshot();
        let mut v: Vec<&Arc<crate::Entry>> = snap.notes().collect();
        let n = limit.min(v.len());
        if n == 0 {
            return Vec::new();
        }
        v.select_nth_unstable_by(n - 1, |a, b| b.mtime_ns.cmp(&a.mtime_ns));
        v.truncate(n);
        v.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns));
        v.into_iter().map(|e| NoteItem { path: e.path.clone(), title: e.title.clone(), mtime_ms: e.mtime_ns / 1_000_000, markdown: true }).collect()
    }

    pub fn stats(&self) -> VaultStats {
        let snap = self.snapshot();
        let now_days = crate::now_ms() / 86_400_000;
        let mut activity = vec![0u32; 182];
        let mut words = 0u64;
        let mut links = 0;
        let mut orphans = 0;
        let mut open = 0u64;
        let mut done = 0u64;
        let mut tags = HashSet::new();
        for e in snap.notes() {
            words += e.word_count as u64;
            links += e.links.iter().filter(|l| l.dst.is_some()).count();
            if e.backlinks.is_empty() && e.links.iter().all(|l| l.dst.is_none()) {
                orphans += 1;
            }
            open += e.tasks_open as u64;
            done += e.tasks_done as u64;
            tags.extend(e.tags.iter());
            let d = now_days - e.mtime_ns / 86_400_000_000_000;
            if (0..182).contains(&d) {
                activity[181 - d as usize] += 1;
            }
        }
        VaultStats {
            notes: snap.notes,
            attachments: snap.attachments,
            folders: snap.dirs.len(),
            words,
            links,
            unresolved: snap.unresolved_targets,
            tags: tags.len(),
            orphans,
            tasks_open: open,
            tasks_done: done,
            activity,
        }
    }

    /// Graph view data. With `center`, a local graph of `depth` hops; else the global graph, with
    /// level of detail: above `max_nodes`, Leiden clusters are drawn as aggregate nodes.
    pub fn graph(&self, center: Option<&str>, depth: usize, max_nodes: usize) -> Result<GraphData> {
        let snap = self.snapshot();
        let layout = self.ensure_layout(&snap);
        let pos = |id: u32| layout.as_ref().and_then(|l| l.pos.get(id as usize).copied()).unwrap_or([0.0, 0.0]);
        let node = |e: &crate::Entry| GraphNode {
            id: e.id,
            title: e.title.clone(),
            path: e.path.clone(),
            x: pos(e.id)[0],
            y: pos(e.id)[1],
            rank: snap.pagerank_norm(e.id),
            cluster: snap.cluster(e.id).unwrap_or(0),
            degree: (e.backlinks.len() + e.links.iter().filter(|l| l.dst.is_some()).count()) as u32,
        };
        let nclusters = snap.clusters.iter().filter(|&&c| c != u32::MAX).max().map(|m| *m as usize + 1).unwrap_or(0);
        if let Some(c) = center {
            let e = snap.by_path(c).ok_or_else(|| crate::CoreError::NotFound(c.into()))?;
            let (g, _local, ids) = self.local_graph(&snap, e.id, depth.clamp(1, 3), max_nodes);
            let mut nodes: Vec<GraphNode> = ids.iter().filter_map(|&id| snap.get(id)).map(|e| node(e)).collect();
            // Local layout: a short force run seeded from global positions keeps it readable.
            let active = vec![true; ids.len()];
            let mut l = vault_graph::Layout::seeded(&g, &active, None);
            for (i, n) in nodes.iter().enumerate() {
                if n.x != 0.0 || n.y != 0.0 {
                    l.pos[i] = [n.x, n.y];
                }
            }
            l.run(&g, &active, &vault_graph::LayoutParams::default(), 300, 0.01);
            for (i, n) in nodes.iter_mut().enumerate() {
                n.x = l.pos[i][0];
                n.y = l.pos[i][1];
            }
            let edges = g.undirected_edges().map(|(a, b, _)| (ids[a as usize], ids[b as usize])).collect();
            return Ok(GraphData { nodes, edges, center: Some(e.id), clusters: nclusters, aggregated: false });
        }
        let notes: Vec<&Arc<crate::Entry>> = snap.notes().collect();
        if notes.len() <= max_nodes {
            let ids: HashSet<u32> = notes.iter().map(|e| e.id).collect();
            let mut edges = Vec::new();
            for e in &notes {
                for l in &e.links {
                    if let Some(d) = l.dst {
                        if ids.contains(&d) && d != e.id {
                            edges.push((e.id.min(d), e.id.max(d)));
                        }
                    }
                }
            }
            edges.sort_unstable();
            edges.dedup();
            return Ok(GraphData { nodes: notes.iter().map(|e| node(e)).collect(), edges, center: None, clusters: nclusters, aggregated: false });
        }
        // Level of detail: one node per cluster at its members' centroid.
        let mut agg: HashMap<u32, (f32, f32, usize, f32, &str, String)> = HashMap::new();
        for e in &notes {
            let c = snap.cluster(e.id).unwrap_or(0);
            let p = pos(e.id);
            let r = snap.pagerank_norm(e.id);
            let a = agg.entry(c).or_insert((0.0, 0.0, 0, -1.0, "", String::new()));
            a.0 += p[0];
            a.1 += p[1];
            a.2 += 1;
            if r > a.3 {
                a.3 = r;
                a.4 = &e.title;
                a.5 = e.path.clone();
            }
        }
        let mut cedges: HashMap<(u32, u32), u32> = HashMap::new();
        for e in &notes {
            let ca = snap.cluster(e.id).unwrap_or(0);
            for l in &e.links {
                if let Some(cb) = l.dst.and_then(|d| snap.cluster(d)) {
                    if ca != cb {
                        *cedges.entry((ca.min(cb), ca.max(cb))).or_default() += 1;
                    }
                }
            }
        }
        let mut nodes: Vec<GraphNode> = agg
            .into_iter()
            .map(|(c, (x, y, n, _, title, path))| GraphNode { id: c, title: format!("{title} +{}", n.saturating_sub(1)), path, x: x / n as f32, y: y / n as f32, rank: n as f32, cluster: c, degree: n as u32 })
            .collect();
        let maxn = nodes.iter().map(|n| n.rank).fold(1.0, f32::max);
        nodes.iter_mut().for_each(|n| n.rank /= maxn);
        nodes.sort_by(|a, b| b.degree.cmp(&a.degree));
        let mut edges: Vec<(u32, u32)> = cedges.into_iter().filter(|(_, w)| *w >= 2).map(|(k, _)| k).collect();
        edges.sort_unstable();
        Ok(GraphData { nodes, edges, center: None, clusters: nclusters, aggregated: true })
    }

    /// Global layout: loaded from layout.bin, extended for new nodes, run until frozen.
    fn ensure_layout(&self, snap: &crate::Snapshot) -> Option<vault_graph::Layout> {
        let mut guard = self.0.layout.lock();
        let n = snap.entries.len();
        let fresh = guard.as_ref().is_some_and(|l| l.frozen && l.pos.len() == n);
        if fresh {
            return guard.clone();
        }
        let prev = guard.take().or_else(|| vault_graph::Layout::load(&self.0.app_dir.join("layout.bin")).ok());
        let (g, active) = self.0.state.lock().graph();
        let mut l = vault_graph::Layout::seeded(&g, &active, prev.as_ref());
        let started_from_prev = prev.is_some();
        let budget = std::time::Instant::now();
        let params = vault_graph::LayoutParams::default();
        let max_steps = if started_from_prev { 150 } else { 600 };
        let mut steps = 0;
        while steps < max_steps && budget.elapsed() < std::time::Duration::from_millis(2500) {
            if l.step(&g, &active, &params) < 0.02 {
                break;
            }
            steps += 1;
        }
        l.frozen = true;
        *guard = Some(l);
        self.0.dirty_persist.store(true, Ordering::SeqCst);
        guard.clone()
    }
}

fn passes_filters(e: &crate::Entry, pq: &vault_search::ParsedQuery) -> bool {
    use vault_search::Clause;
    let tag_ok = |t: &str| e.tags.iter().any(|x| x == t || x.starts_with(&format!("{t}/")));
    let path_ok = |p: &str| {
        let lp = e.path.to_lowercase();
        lp == p || lp.starts_with(&format!("{p}/"))
    };
    for c in &pq.must {
        match c {
            Clause::Tag(t) if !tag_ok(t) => return false,
            Clause::Path(p) if !path_ok(p) => return false,
            _ => {}
        }
    }
    for c in &pq.exclude {
        match c {
            Clause::Tag(t) if tag_ok(t) => return false,
            Clause::Path(p) if path_ok(p) => return false,
            Clause::Term(t) if e.title.to_lowercase().contains(t.as_str()) => return false,
            _ => {}
        }
    }
    true
}

fn src_frontmatter_end(text: &str) -> u32 {
    if !text.starts_with("---") {
        return 0;
    }
    text[3..].find("\n---").map(|i| (i + 7) as u32).unwrap_or(0)
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
