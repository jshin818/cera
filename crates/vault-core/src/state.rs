//! Writer-side engine state and the immutable [`Snapshot`] it publishes.
//!
//! Only engine threads mutate [`State`] (under one mutex). Readers load an `Arc<Snapshot>` via
//! arc-swap and never block. Publishing clones a vector of `Arc<Entry>` pointers (cheap), and
//! entries are replaced, never mutated.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use vault_parse::{Chunk, LinkKind, LinkResolver, ParsedNote, Resolved, Subpath};
use vault_search::{Frecency, IndexDoc};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OutLink {
    pub target: String,
    pub subpath: Option<Subpath>,
    pub display: Option<String>,
    pub embed: bool,
    pub start: u32,
    pub end: u32,
    pub dst: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entry {
    pub id: u32,
    pub path: String,
    pub markdown: bool,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: u64,
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub headings: Vec<(u8, String, u32)>,
    pub links: Vec<OutLink>,
    pub block_ids: Vec<String>,
    pub word_count: u32,
    pub tasks_open: u32,
    pub tasks_done: u32,
    #[serde(skip)]
    pub frontmatter: Option<serde_json::Value>,
    /// Sorted, unique source ids that link or embed this file.
    pub backlinks: Vec<u32>,
}

impl Entry {
    pub fn folder(&self) -> &str {
        self.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
    }
}

/// Immutable view of the vault. Cheap to clone (all heavy fields are behind Arcs).
#[derive(Clone, Default)]
pub struct Snapshot {
    pub generation: u64,
    pub entries: Arc<Vec<Option<Arc<Entry>>>>,
    pub by_path: Arc<HashMap<String, u32>>,
    pub dirs: Arc<Vec<String>>,
    pub unresolved: Arc<HashMap<String, Vec<u32>>>,
    pub pagerank: Arc<Vec<f32>>,
    pub pagerank_max: f32,
    pub clusters: Arc<Vec<u32>>,
    pub frecency: Arc<Vec<Frecency>>,
    pub frecency_max: f32,
    pub notes: usize,
    pub attachments: usize,
}

impl Snapshot {
    pub fn get(&self, id: u32) -> Option<&Arc<Entry>> {
        self.entries.get(id as usize).and_then(|e| e.as_ref())
    }

    pub fn by_path(&self, path: &str) -> Option<&Arc<Entry>> {
        self.by_path.get(path).and_then(|&id| self.get(id))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<Entry>> {
        self.entries.iter().filter_map(|e| e.as_ref())
    }

    pub fn notes(&self) -> impl Iterator<Item = &Arc<Entry>> {
        self.iter().filter(|e| e.markdown)
    }

    pub fn pagerank_norm(&self, id: u32) -> f32 {
        if self.pagerank_max <= 0.0 {
            return 0.0;
        }
        self.pagerank.get(id as usize).copied().unwrap_or(0.0) / self.pagerank_max
    }

    pub fn frecency_norm(&self, id: u32, now_ms: i64) -> f32 {
        if self.frecency_max <= 0.0 {
            return 0.0;
        }
        (self.frecency.get(id as usize).map(|f| f.decayed(now_ms)).unwrap_or(0.0) / self.frecency_max).min(1.0)
    }

    pub fn cluster(&self, id: u32) -> Option<u32> {
        self.clusters.get(id as usize).copied().filter(|&c| c != u32::MAX)
    }
}

/// One file to (re)index, produced by the parallel read+parse stage.
pub struct FileUpdate {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: u64,
    /// Parsed note for Markdown files whose content changed; `None` means only the stat changed
    /// (or an attachment).
    pub parsed: Option<ParsedNote>,
    pub chunks: Vec<Chunk>,
    pub minhash: Option<[u32; vault_graph::PERMS]>,
}

/// What an `apply` changed, for the caller to propagate into tantivy/redb/vectors.
#[derive(Default)]
pub struct Applied {
    pub index_docs: Vec<IndexDoc>,
    pub removed_ids: Vec<u32>,
    /// (file id, chunks) to (re)embed
    pub embed: Vec<(u32, Vec<Chunk>)>,
    pub minhash: Vec<(u32, Option<[u32; vault_graph::PERMS]>)>,
    pub store: vault_store::StoreBatch,
    pub titles_changed: bool,
    pub graph_changed: bool,
    pub changed_paths: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub entries: Vec<Option<Arc<Entry>>>,
    pub by_path: HashMap<String, u32>,
    pub resolver: LinkResolver,
    /// lowercase link-target name (last path segment, without .md) -> sources using that name
    pub by_target_name: HashMap<String, HashSet<u32>>,
    pub unresolved: HashMap<String, Vec<u32>>,
    pub dirs: BTreeSet<String>,
    pub frecency: Vec<Frecency>,
    pub pagerank: Vec<f32>,
    pub clusters: Vec<u32>,
    pub generation: u64,
}

pub fn target_name(target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() {
        return None;
    }
    let last = t.rsplit('/').next().unwrap_or(t).to_lowercase();
    Some(last.strip_suffix(".md").map(str::to_string).unwrap_or(last))
}

fn name_keys(path: &str, aliases: &[String]) -> Vec<String> {
    let mut v = Vec::with_capacity(aliases.len() + 2);
    let file = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    if let Some(stem) = file.strip_suffix(".md") {
        v.push(stem.to_string());
    }
    v.push(file);
    v.extend(aliases.iter().map(|a| a.to_lowercase()));
    v
}

fn link_kind_code(l: &OutLink) -> u8 {
    match (&l.subpath, l.embed) {
        (_, true) => 1,
        (Some(Subpath::Heading(_)), _) => 2,
        (Some(Subpath::Block(_)), _) => 3,
        _ => 0,
    }
}

impl State {
    fn alloc_id(&mut self, path: &str) -> (u32, bool) {
        if let Some(&id) = self.by_path.get(path) {
            return (id, false);
        }
        let id = self.entries.len() as u32;
        self.entries.push(None);
        self.frecency.push(Frecency::default());
        self.by_path.insert(path.to_string(), id);
        (id, true)
    }

    fn add_dirs_of(&mut self, path: &str) {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for p in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(p);
            self.dirs.insert(acc.clone());
        }
    }

    fn resolve_links(&self, id: u32, path: &str, links: &mut [OutLink]) {
        for l in links.iter_mut() {
            l.dst = match self.resolver.resolve(&l.target, path, id) {
                Resolved::File(d) => Some(d),
                Resolved::Unresolved => None,
            };
        }
    }

    fn set_backlinks(&mut self, dst: u32, add: &[u32], remove: &[u32], touched: &mut HashSet<u32>) {
        let Some(Some(e)) = self.entries.get(dst as usize) else { return };
        let mut b = e.backlinks.clone();
        b.retain(|s| !remove.contains(s));
        for &s in add {
            if let Err(pos) = b.binary_search(&s) {
                b.insert(pos, s);
            }
        }
        if b != e.backlinks {
            let mut ne = (**e).clone();
            ne.backlinks = b;
            self.entries[dst as usize] = Some(Arc::new(ne));
            touched.insert(dst);
        }
    }

    /// Apply removals and updates atomically; keeps links, backlinks and unresolved targets consistent.
    pub fn apply(&mut self, updates: Vec<FileUpdate>, removals: Vec<String>) -> Applied {
        let mut out = Applied::default();
        let mut affected_names: HashSet<String> = HashSet::new();
        // dst -> (added srcs, removed srcs)
        let mut back_add: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut back_rm: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut touched_backlinks: HashSet<u32> = HashSet::new();
        let mut unresolved_dirty: HashSet<String> = HashSet::new();

        let drop_outgoing = |st: &mut State, src: u32, links: &[OutLink], back_rm: &mut HashMap<u32, Vec<u32>>, unresolved_dirty: &mut HashSet<String>| {
            for l in links {
                match l.dst {
                    Some(d) if d != src => back_rm.entry(d).or_default().push(src),
                    Some(_) => {}
                    None => {
                        let key = l.target.to_lowercase();
                        if let Some(v) = st.unresolved.get_mut(&key) {
                            v.retain(|&s| s != src);
                            unresolved_dirty.insert(key);
                        }
                    }
                }
                if let Some(n) = target_name(&l.target) {
                    if let Some(set) = st.by_target_name.get_mut(&n) {
                        set.remove(&src);
                    }
                }
            }
        };

        // 1. removals
        for path in removals {
            let Some(id) = self.by_path.remove(&path) else { continue };
            let Some(old) = self.entries[id as usize].take() else { continue };
            self.resolver.remove_file(id, &old.path);
            affected_names.extend(name_keys(&old.path, &old.aliases));
            drop_outgoing(self, id, &old.links, &mut back_rm, &mut unresolved_dirty);
            out.removed_ids.push(id);
            out.store.files.push((id, None));
            out.store.notes.push((id, None));
            out.store.links.push((id, Vec::new()));
            out.store.chunks.push((id, Vec::new()));
            out.minhash.push((id, None));
            if old.markdown {
                out.titles_changed = true;
            }
            out.graph_changed = true;
            out.changed_paths.push(path);
            // Sources that pointed at it now need re-resolution (handled via affected names).
            for &s in &old.backlinks {
                if let Some(Some(se)) = self.entries.get(s as usize) {
                    for l in &se.links {
                        if let Some(n) = target_name(&l.target) {
                            affected_names.insert(n);
                        }
                    }
                }
            }
        }

        // 2. upserts (register files first so links between new files resolve)
        let mut reparsed: Vec<(u32, FileUpdate)> = Vec::new();
        for u in updates {
            let (id, is_new) = self.alloc_id(&u.rel);
            self.add_dirs_of(&u.rel);
            let old = self.entries[id as usize].clone();
            let new_aliases: Vec<String> = u.parsed.as_ref().map(|p| p.aliases.clone()).unwrap_or_else(|| old.as_ref().map(|o| o.aliases.clone()).unwrap_or_default());
            let aliases_changed = old.as_ref().is_none_or(|o| o.aliases != new_aliases);
            if is_new || aliases_changed {
                if !is_new {
                    self.resolver.remove_file(id, &u.rel);
                }
                self.resolver.add_file(id, &u.rel);
                for a in &new_aliases {
                    self.resolver.add_alias(id, u.rel.len() as u32, a);
                }
                affected_names.extend(name_keys(&u.rel, &new_aliases));
                if let Some(o) = &old {
                    affected_names.extend(o.aliases.iter().map(|a| a.to_lowercase()));
                }
                if vault_fs::FileKind::of(&u.rel) == vault_fs::FileKind::Markdown {
                    out.titles_changed = true;
                }
            }
            out.changed_paths.push(u.rel.clone());
            out.store.files.push((id, Some(vault_store::FileRec { path: u.rel.clone(), size: u.size, mtime_ns: u.mtime_ns, xxh3: u.hash, markdown: vault_fs::FileKind::of(&u.rel) == vault_fs::FileKind::Markdown })));
            if u.parsed.is_none() {
                // stat-only (or attachment)
                let e = match old {
                    Some(o) => Entry { size: u.size, mtime_ns: u.mtime_ns, hash: u.hash, ..(*o).clone() },
                    None => Entry {
                        id,
                        path: u.rel.clone(),
                        markdown: vault_fs::FileKind::of(&u.rel) == vault_fs::FileKind::Markdown,
                        size: u.size,
                        mtime_ns: u.mtime_ns,
                        hash: u.hash,
                        title: vault_parse::title_from_path(&u.rel),
                        aliases: Vec::new(),
                        tags: Vec::new(),
                        headings: Vec::new(),
                        links: Vec::new(),
                        block_ids: Vec::new(),
                        word_count: 0,
                        tasks_open: 0,
                        tasks_done: 0,
                        frontmatter: None,
                        backlinks: Vec::new(),
                    },
                };
                self.entries[id as usize] = Some(Arc::new(e));
                continue;
            }
            if let Some(o) = &old {
                drop_outgoing(self, id, &o.links, &mut back_rm, &mut unresolved_dirty);
            }
            reparsed.push((id, u));
        }

        // 3. build entries for reparsed notes
        for (id, u) in reparsed {
            let p = u.parsed.expect("reparsed has parse");
            let old_back = self.entries[id as usize].as_ref().map(|o| o.backlinks.clone()).unwrap_or_default();
            let mut links: Vec<OutLink> = p
                .links
                .iter()
                .map(|l| OutLink { target: l.target.clone(), subpath: l.subpath.clone(), display: l.display.clone(), embed: l.kind == LinkKind::Embed, start: l.start, end: l.end, dst: None })
                .collect();
            self.resolve_links(id, &u.rel, &mut links);
            let e = Entry {
                id,
                path: u.rel.clone(),
                markdown: true,
                size: u.size,
                mtime_ns: u.mtime_ns,
                hash: u.hash,
                title: p.title.clone(),
                aliases: p.aliases.clone(),
                tags: p.tags.clone(),
                headings: p.headings.iter().map(|h| (h.level, h.text.clone(), h.offset)).collect(),
                links,
                block_ids: p.block_ids.iter().map(|b| b.0.clone()).collect(),
                word_count: p.word_count,
                tasks_open: p.tasks.iter().filter(|t| !t.done).count() as u32,
                tasks_done: p.tasks.iter().filter(|t| t.done).count() as u32,
                frontmatter: p.frontmatter.clone(),
                backlinks: old_back,
            };
            self.register_outgoing(&e, &mut back_add, &mut unresolved_dirty);
            out.index_docs.push(IndexDoc {
                file_id: id,
                path: u.rel.clone(),
                title: p.title.clone(),
                aliases: p.aliases.clone(),
                headings: p.headings.iter().map(|h| h.text.clone()).collect(),
                tags: p.tags.clone(),
                body: p.body_text,
                mtime_ms: (u.mtime_ns / 1_000_000).max(0) as u64,
            });
            out.store.notes.push((
                id,
                Some(vault_store::NoteRec {
                    title: e.title.clone(),
                    aliases: e.aliases.clone(),
                    tags: e.tags.clone(),
                    frontmatter_json: e.frontmatter.as_ref().map(|f| f.to_string()).unwrap_or_default(),
                    headings: e.headings.clone(),
                    word_count: e.word_count,
                }),
            ));
            out.store.links.push((id, link_records(&e)));
            out.embed.push((id, u.chunks));
            out.minhash.push((id, u.minhash));
            out.graph_changed = true;
            self.entries[id as usize] = Some(Arc::new(e));
        }

        // 4. re-resolve sources affected by name changes
        let mut sources: HashSet<u32> = HashSet::new();
        for n in &affected_names {
            if let Some(s) = self.by_target_name.get(n) {
                sources.extend(s.iter().copied());
            }
        }
        for src in sources {
            let Some(Some(e)) = self.entries.get(src as usize) else { continue };
            let mut links = e.links.clone();
            self.resolve_links(src, &e.path, &mut links);
            if links == e.links {
                continue;
            }
            let old_links = e.links.clone();
            let mut ne = (**e).clone();
            ne.links = links;
            // Undo old registrations then register new.
            for l in &old_links {
                match l.dst {
                    Some(d) if d != src => back_rm.entry(d).or_default().push(src),
                    Some(_) => {}
                    None => {
                        let key = l.target.to_lowercase();
                        if let Some(v) = self.unresolved.get_mut(&key) {
                            v.retain(|&s| s != src);
                            unresolved_dirty.insert(key);
                        }
                    }
                }
            }
            self.register_outgoing(&ne, &mut back_add, &mut unresolved_dirty);
            out.store.links.push((src, link_records(&ne)));
            out.graph_changed = true;
            self.entries[src as usize] = Some(Arc::new(ne));
        }

        // 5. backlinks: apply removals then additions (a src may appear in both when links persist)
        let dsts: HashSet<u32> = back_add.keys().chain(back_rm.keys()).copied().collect();
        for d in dsts {
            let add = back_add.remove(&d).unwrap_or_default();
            let rm: Vec<u32> = back_rm.remove(&d).unwrap_or_default().into_iter().filter(|s| !add.contains(s)).collect();
            self.set_backlinks(d, &add, &rm, &mut touched_backlinks);
        }
        for d in touched_backlinks {
            if let Some(Some(e)) = self.entries.get(d as usize) {
                out.store.backlinks.push((d, e.backlinks.clone()));
            }
        }
        for key in unresolved_dirty {
            let v = self.unresolved.get(&key).cloned().unwrap_or_default();
            if v.is_empty() {
                self.unresolved.remove(&key);
            }
            out.store.unresolved.push((key, v));
        }
        if !out.changed_paths.is_empty() {
            self.generation += 1;
        }
        out
    }

    fn register_outgoing(&mut self, e: &Entry, back_add: &mut HashMap<u32, Vec<u32>>, unresolved_dirty: &mut HashSet<String>) {
        for l in &e.links {
            match l.dst {
                Some(d) if d != e.id => back_add.entry(d).or_default().push(e.id),
                Some(_) => {}
                None => {
                    let key = l.target.to_lowercase();
                    let v = self.unresolved.entry(key.clone()).or_default();
                    if let Err(pos) = v.binary_search(&e.id) {
                        v.insert(pos, e.id);
                    }
                    unresolved_dirty.insert(key);
                }
            }
            if let Some(n) = target_name(&l.target) {
                self.by_target_name.entry(n).or_default().insert(e.id);
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = crate::now_ms();
        let pr_max = self.pagerank.iter().copied().fold(0.0, f32::max);
        let fr_max = self.frecency.iter().map(|f| f.decayed(now)).fold(0.0, f32::max);
        let notes = self.entries.iter().flatten().filter(|e| e.markdown).count();
        let total = self.entries.iter().flatten().count();
        Snapshot {
            generation: self.generation,
            entries: Arc::new(self.entries.clone()),
            by_path: Arc::new(self.by_path.clone()),
            dirs: Arc::new(self.dirs.iter().cloned().collect()),
            unresolved: Arc::new(self.unresolved.clone()),
            pagerank: Arc::new(self.pagerank.clone()),
            pagerank_max: pr_max,
            clusters: Arc::new(self.clusters.clone()),
            frecency: Arc::new(self.frecency.clone()),
            frecency_max: fr_max,
            notes,
            attachments: total - notes,
        }
    }

    pub fn graph(&self) -> (vault_graph::Graph, Vec<bool>) {
        let n = self.entries.len();
        let mut edges = Vec::new();
        let mut active = vec![false; n];
        for e in self.entries.iter().flatten() {
            if !e.markdown {
                continue;
            }
            active[e.id as usize] = true;
            for l in &e.links {
                if let Some(d) = l.dst {
                    if self.entries.get(d as usize).and_then(|x| x.as_ref()).is_some_and(|x| x.markdown) {
                        edges.push((e.id, d));
                    }
                }
            }
        }
        (vault_graph::Graph::new(n, &edges), active)
    }
}

fn link_records(e: &Entry) -> Vec<(vault_store::LinkKey, vault_store::LinkRec)> {
    let mut m: HashMap<vault_store::LinkKey, vault_store::LinkRec> = HashMap::new();
    for l in &e.links {
        if let Some(d) = l.dst {
            let r = m.entry((e.id, d, link_kind_code(l))).or_insert(vault_store::LinkRec { count: 0, offsets: Vec::new() });
            r.count += 1;
            r.offsets.push(l.start);
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by_key(|x| x.0);
    v
}
