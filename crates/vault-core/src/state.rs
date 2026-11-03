//! Writer-side engine state and the immutable [`Snapshot`] it publishes.
//!
//! Only engine threads mutate [`State`] (under one mutex). Readers load an `Arc<Snapshot>` via
//! arc-swap and never block. Publishing clones a vector of `Arc<Entry>` pointers (cheap), and
//! entries are replaced, never mutated.

use crate::intern::{Interner, NONE};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use vault_parse::{Chunk, LinkKind, LinkResolver, ParsedNote, Resolved, Subpath};
use vault_search::{Frecency, IndexDoc};

pub const F_EMBED: u8 = 1;
pub const F_HEADING: u8 = 2;
pub const F_BLOCK: u8 = 4;

/// A link inside a note: 20 bytes, no heap. Strings are interned symbols (see [`Interner`]);
/// the rarely-needed subpath and display text share one optional symbol (`sub \x1f display`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutLink {
    /// Target as written (without subpath or display).
    pub target: u32,
    pub dst: u32,
    pub start: u32,
    pub extra: u32,
    pub len: u16,
    pub flags: u8,
}

impl OutLink {
    pub fn end(&self) -> u32 {
        self.start + self.len as u32
    }
    fn extra_parts(&self, syms: &Interner) -> (Option<String>, Option<String>) {
        if self.extra == NONE {
            return (None, None);
        }
        let s = syms.get(self.extra);
        let (a, b) = s.split_once('\u{1f}').unwrap_or((&s, ""));
        ((!a.is_empty()).then(|| a.to_string()), (!b.is_empty()).then(|| b.to_string()))
    }
    pub fn display(&self, syms: &Interner) -> Option<String> {
        self.extra_parts(syms).1
    }
    pub fn dst(&self) -> Option<u32> {
        (self.dst != NONE).then_some(self.dst)
    }
    pub fn embed(&self) -> bool {
        self.flags & F_EMBED != 0
    }
    pub fn subpath(&self, syms: &Interner) -> Option<Subpath> {
        let sub = self.extra_parts(syms).0.unwrap_or_default();
        if self.flags & F_HEADING != 0 {
            Some(Subpath::Heading(sub))
        } else if self.flags & F_BLOCK != 0 {
            Some(Subpath::Block(sub))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    pub links: Vec<OutLink>,
    pub word_count: u32,
    pub tasks_open: u32,
    pub tasks_done: u32,
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
    pub syms: Arc<Interner>,
    pub entries: Arc<Vec<Option<Arc<Entry>>>>,
    /// Backlinks per file id (sorted, unique source ids), kept apart from entries so a new link
    /// replaces one small vector instead of cloning the target's whole entry.
    pub back: Arc<Vec<Arc<Vec<u32>>>>,
    pub by_path: Arc<HashMap<String, u32>>,
    pub dirs: Arc<Vec<String>>,
    pub unresolved_targets: usize,
    pub pagerank: Arc<Vec<f32>>,
    pub pagerank_max: f32,
    pub clusters: Arc<Vec<u32>>,
    pub frecency: Arc<Vec<Frecency>>,
    pub frecency_max: f32,
    pub notes: usize,
    pub attachments: usize,
}

impl Snapshot {
    pub fn sym(&self, id: u32) -> Arc<str> {
        self.syms.get(id)
    }

    pub fn get(&self, id: u32) -> Option<&Arc<Entry>> {
        self.entries.get(id as usize).and_then(|e| e.as_ref())
    }

    pub fn backlinks(&self, id: u32) -> &[u32] {
        self.back.get(id as usize).map(|v| v.as_slice()).unwrap_or(&[])
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
    pub back: Vec<Arc<Vec<u32>>>,
    /// Shared with snapshots; copied on write only when paths change.
    pub by_path: Arc<HashMap<String, u32>>,
    pub resolver: LinkResolver,
    /// lowercase link-target name (last path segment, without .md) -> sources using that name
    /// Unresolved link name (lower-cased last segment) symbol -> sources (sorted).
    pub unresolved: HashMap<u32, Vec<u32>>,
    /// target symbol -> name symbol (lower-cased last path segment without .md), computed lazily.
    pub name_of: HashMap<u32, u32>,
    pub syms: Arc<Interner>,
    pub dirs: BTreeSet<String>,
    pub frecency: Vec<Frecency>,
    pub pagerank: Vec<f32>,
    pub clusters: Vec<u32>,
    pub generation: u64,
    /// Ids allocated (and registered with the resolver) before their content was indexed, so the
    /// first index resolves every link once instead of re-resolving as batches arrive.
    pub preregistered: HashSet<u32>,
    /// During the first index, intermediate snapshots omit the backlink table so updating it never
    /// copies vectors still shared with a published snapshot.
    pub defer_backlinks: bool,
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
    if l.flags & F_EMBED != 0 {
        1
    } else if l.flags & F_HEADING != 0 {
        2
    } else if l.flags & F_BLOCK != 0 {
        3
    } else {
        0
    }
}

fn remove_sorted(v: &mut Vec<u32>, x: u32) {
    if let Ok(i) = v.binary_search(&x) {
        v.remove(i);
    }
}

fn insert_sorted(v: &mut Vec<u32>, x: u32) {
    if let Err(i) = v.binary_search(&x) {
        v.insert(i, x);
    }
}

/// Build an interned link from a parsed one.
pub fn make_link(syms: &Interner, l: &vault_parse::Link) -> OutLink {
    let (sub, mut flags) = match &l.subpath {
        Some(Subpath::Heading(h)) => (h.as_str(), F_HEADING),
        Some(Subpath::Block(b)) => (b.as_str(), F_BLOCK),
        None => ("", 0),
    };
    if l.kind == LinkKind::Embed {
        flags |= F_EMBED;
    }
    let display = l.display.as_deref().unwrap_or("");
    let extra = if sub.is_empty() && display.is_empty() { NONE } else { syms.intern(&format!("{sub}\u{1f}{display}")) };
    OutLink { target: syms.intern(&l.target), dst: NONE, start: l.start, extra, len: (l.end - l.start).min(u16::MAX as u32) as u16, flags }
}

impl State {
    /// Name symbol for a target symbol (lower-cased last segment without .md); NONE for "".
    pub fn name_sym(&mut self, target: u32) -> u32 {
        if let Some(&n) = self.name_of.get(&target) {
            return n;
        }
        let n = target_name(&self.syms.get(target)).map(|x| self.syms.intern(&x)).unwrap_or(NONE);
        self.name_of.insert(target, n);
        n
    }

    fn alloc_id(&mut self, path: &str) -> (u32, bool) {
        if let Some(&id) = self.by_path.get(path) {
            return (id, false);
        }
        let id = self.entries.len() as u32;
        self.entries.push(None);
        self.back.push(Arc::default());
        self.frecency.push(Frecency::default());
        Arc::make_mut(&mut self.by_path).insert(path.to_string(), id);
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

    /// Register paths before indexing them (see `preregistered`).
    pub fn preregister(&mut self, paths: &[&str]) {
        for p in paths {
            if self.by_path.contains_key(*p) {
                continue;
            }
            let (id, _) = self.alloc_id(p);
            self.resolver.add_file(id, p);
            self.add_dirs_of(p);
            self.preregistered.insert(id);
        }
    }

    fn resolve_links(&self, id: u32, path: &str, links: &mut [OutLink]) {
        for l in links.iter_mut() {
            l.dst = match self.resolver.resolve(&self.syms.get(l.target), path, id) {
                Resolved::File(d) => d,
                Resolved::Unresolved => NONE,
            };
        }
    }

    fn set_backlinks(&mut self, dst: u32, add: &[u32], remove: &[u32], touched: &mut HashSet<u32>) {
        let Some(slot) = self.back.get_mut(dst as usize) else { return };
        let mut changed = false;
        if remove.is_empty() && !add.is_empty() && slot.last().is_none_or(|l| add[0] > *l) {
            // Common case in bulk indexing: new sources have higher ids; append.
            Arc::make_mut(slot).extend_from_slice(add);
            touched.insert(dst);
            return;
        }
        if remove.iter().any(|s| slot.binary_search(s).is_ok()) || add.iter().any(|s| slot.binary_search(s).is_err()) {
            let b = Arc::make_mut(slot);
            for &s in remove {
                if let Ok(i) = b.binary_search(&s) {
                    b.remove(i);
                    changed = true;
                }
            }
            for &s in add {
                if let Err(i) = b.binary_search(&s) {
                    b.insert(i, s);
                    changed = true;
                }
            }
        }
        if changed {
            touched.insert(dst);
        }
    }

    /// Apply removals and updates atomically; keeps links, backlinks and unresolved targets consistent.
    pub fn apply(&mut self, updates: Vec<FileUpdate>, removals: Vec<String>) -> Applied {
        let mut out = Applied::default();
        let mut affected_names: HashSet<String> = HashSet::new();
        // Names that only gained/lost an alias: only links using that name can change.
        let mut affected_aliases: HashSet<String> = HashSet::new();
        // dst -> (added srcs, removed srcs)
        // (dst, src) pairs; sorted and grouped once at the end (cheaper than a map per link).
        let mut back_add: Vec<(u32, u32)> = Vec::new();
        let mut back_rm: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut touched_backlinks: HashSet<u32> = HashSet::new();
        let mut unresolved_dirty: HashSet<u32> = HashSet::new();
        let mut removed_sources: Vec<u32> = Vec::new();

        let drop_outgoing = |st: &mut State, src: u32, links: &[OutLink], back_rm: &mut HashMap<u32, Vec<u32>>, unresolved_dirty: &mut HashSet<u32>| {
            for l in links {
                match l.dst() {
                    Some(d) if d != src => back_rm.entry(d).or_default().push(src),
                    Some(_) => {}
                    None => {
                        let k = st.name_sym(l.target);
                        if let Some(v) = st.unresolved.get_mut(&k) {
                            remove_sorted(v, src);
                            unresolved_dirty.insert(k);
                        }
                    }
                }
            }
        };

        // 1. removals
        for path in removals {
            let Some(id) = Arc::make_mut(&mut self.by_path).remove(&path) else { continue };
            let Some(old) = self.entries[id as usize].take() else {
                if self.preregistered.remove(&id) {
                    self.resolver.remove_file(id, &path);
                }
                continue;
            };
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
            // Sources that pointed at it now need re-resolution.
            removed_sources.extend(self.back[id as usize].iter().copied());
        }

        // 2. upserts (register files first so links between new files resolve)
        let mut reparsed: Vec<(u32, FileUpdate)> = Vec::new();
        for u in updates {
            let (id, is_new) = self.alloc_id(&u.rel);
            self.add_dirs_of(&u.rel);
            let old = self.entries[id as usize].clone();
            let new_aliases: Vec<String> =
                u.parsed.as_ref().map(|p| p.aliases.clone()).unwrap_or_else(|| old.as_ref().map(|o| o.aliases.clone()).unwrap_or_default());
            let aliases_changed = old.as_ref().is_none_or(|o| o.aliases != new_aliases);
            if self.preregistered.remove(&id) {
                // Name already known to the resolver; only aliases are new information.
                for a in &new_aliases {
                    self.resolver.add_alias(id, u.rel.len() as u32, a);
                    affected_aliases.insert(a.to_lowercase());
                }
                if vault_fs::FileKind::of(&u.rel) == vault_fs::FileKind::Markdown {
                    out.titles_changed = true;
                }
            } else if is_new || aliases_changed {
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
            out.store.files.push((
                id,
                Some(vault_store::FileRec {
                    path: u.rel.clone(),
                    size: u.size,
                    mtime_ns: u.mtime_ns,
                    xxh3: u.hash,
                    markdown: vault_fs::FileKind::of(&u.rel) == vault_fs::FileKind::Markdown,
                }),
            ));
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
                        links: Vec::new(),
                        word_count: 0,
                        tasks_open: 0,
                        tasks_done: 0,
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

        // 3. build entries for reparsed notes: the expensive part (resolution, clones, records)
        //    runs in parallel against the read-only resolver; registration is serial.
        use rayon::prelude::*;
        let built: Vec<Built> = {
            let this = &*self;
            reparsed
                .into_par_iter()
                .map(|(id, u)| {
                    let p = u.parsed.expect("reparsed has parse");
                    let mut links: Vec<OutLink> = p.links.iter().map(|l| make_link(&this.syms, l)).collect();
                    this.resolve_links(id, &u.rel, &mut links);
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
                        links,
                        word_count: p.word_count,
                        tasks_open: p.tasks.iter().filter(|t| !t.done).count() as u32,
                        tasks_done: p.tasks.iter().filter(|t| t.done).count() as u32,
                    };
                    let note_headings: Vec<(u8, String, u32)> = p.headings.iter().map(|h| (h.level, h.text.clone(), h.offset)).collect();
                    let fm_json = p.frontmatter.as_ref().map(|f| f.to_string()).unwrap_or_default();
                    let doc = IndexDoc {
                        file_id: id,
                        path: u.rel.clone(),
                        title: p.title,
                        aliases: p.aliases,
                        headings: p.headings.into_iter().map(|h| h.text).collect(),
                        tags: p.tags,
                        body: p.body_text,
                        mtime_ms: (u.mtime_ns / 1_000_000).max(0) as u64,
                    };
                    let note = vault_store::NoteRec {
                        title: e.title.clone(),
                        aliases: e.aliases.clone(),
                        tags: e.tags.clone(),
                        frontmatter_json: fm_json,
                        headings: note_headings,
                        word_count: e.word_count,
                    };
                    let links = link_records(&e);
                    Built { e, doc, note, links, chunks: u.chunks, minhash: u.minhash }
                })
                .collect()
        };
        out.index_docs.reserve(built.len());
        for b in built {
            let Built { e, doc, note, links, chunks, minhash } = b;
            let id = e.id;
            self.register_outgoing(&e, &mut back_add, &mut unresolved_dirty);
            out.index_docs.push(doc);
            out.store.notes.push((id, Some(note)));
            out.store.links.push((id, links));
            out.embed.push((id, chunks));
            out.minhash.push((id, minhash));
            out.graph_changed = true;
            self.entries[id as usize] = Some(Arc::new(e));
        }

        // 4. re-resolve sources affected by name changes
        // Affected sources: unresolved links using a changed name, plus everything currently linking
        // to a file with that name (a new file may now be the shorter-path match).
        let mut sources: HashSet<u32> = removed_sources.into_iter().collect();
        for n in &affected_names {
            if let Some(s) = self.syms.lookup(n).and_then(|sym| self.unresolved.get(&sym)) {
                sources.extend(s.iter().copied());
            }
            for f in self.resolver.candidates(n) {
                if let Some(b) = self.back.get(f as usize) {
                    sources.extend(b.iter().copied());
                }
            }
        }
        for n in &affected_aliases {
            if affected_names.contains(n) {
                continue;
            }
            if let Some(s) = self.syms.lookup(n).and_then(|sym| self.unresolved.get(&sym)) {
                sources.extend(s.iter().copied());
            }
            // A file name always beats an alias, so only other alias holders can lose sources.
            for f in self.resolver.alias_candidates(n) {
                if let Some(b) = self.back.get(f as usize) {
                    sources.extend(b.iter().copied());
                }
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
                match l.dst() {
                    Some(d) if d != src => back_rm.entry(d).or_default().push(src),
                    Some(_) => {}
                    None => {
                        let k = self.name_sym(l.target);
                        if let Some(v) = self.unresolved.get_mut(&k) {
                            remove_sorted(v, src);
                            unresolved_dirty.insert(k);
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
        back_add.sort_unstable();
        back_add.dedup();
        let mut i = 0;
        while i < back_add.len() {
            let d = back_add[i].0;
            let mut j = i;
            while j < back_add.len() && back_add[j].0 == d {
                j += 1;
            }
            let add: Vec<u32> = back_add[i..j].iter().map(|x| x.1).collect();
            let rm: Vec<u32> = back_rm.remove(&d).unwrap_or_default().into_iter().filter(|s| add.binary_search(s).is_err()).collect();
            self.set_backlinks(d, &add, &rm, &mut touched_backlinks);
            i = j;
        }
        for (d, rm) in std::mem::take(&mut back_rm) {
            self.set_backlinks(d, &[], &rm, &mut touched_backlinks);
        }
        for d in touched_backlinks {
            out.store.backlinks.push((d, self.back[d as usize].to_vec()));
        }
        for key in unresolved_dirty {
            let v = self.unresolved.get(&key).cloned().unwrap_or_default();
            if v.is_empty() {
                self.unresolved.remove(&key);
            }
            out.store.unresolved.push((self.syms.get(key).to_string(), v));
        }
        if !out.changed_paths.is_empty() {
            self.generation += 1;
        }
        out
    }

    fn register_outgoing(&mut self, e: &Entry, back_add: &mut Vec<(u32, u32)>, unresolved_dirty: &mut HashSet<u32>) {
        for l in &e.links {
            match l.dst() {
                Some(d) if d != e.id => back_add.push((d, e.id)),
                Some(_) => {}
                None => {
                    let k = self.name_sym(l.target);
                    if k != NONE {
                        insert_sorted(self.unresolved.entry(k).or_default(), e.id);
                        unresolved_dirty.insert(k);
                    }
                }
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = crate::now_ms();
        let pr_max = self.pagerank.iter().copied().fold(0.0, f32::max);
        let fr_max = self.frecency.iter().filter(|f| f.score > 0.0).map(|f| f.decayed(now)).fold(0.0, f32::max);
        let notes = self.entries.iter().flatten().filter(|e| e.markdown).count();
        let total = self.entries.iter().flatten().count();
        Snapshot {
            generation: self.generation,
            syms: self.syms.clone(),
            entries: Arc::new(self.entries.clone()),
            back: if self.defer_backlinks { Arc::default() } else { Arc::new(self.back.clone()) },
            by_path: self.by_path.clone(),
            dirs: Arc::new(self.dirs.iter().cloned().collect()),
            unresolved_targets: self.unresolved.len(),
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
                if let Some(d) = l.dst()
                    && self.entries.get(d as usize).and_then(|x| x.as_ref()).is_some_and(|x| x.markdown)
                {
                    edges.push((e.id, d));
                }
            }
        }
        (vault_graph::Graph::new(n, &edges), active)
    }
}

struct Built {
    e: Entry,
    doc: IndexDoc,
    note: vault_store::NoteRec,
    links: Vec<(vault_store::LinkKey, vault_store::LinkRec)>,
    chunks: Vec<Chunk>,
    minhash: Option<[u32; vault_graph::PERMS]>,
}

fn link_records(e: &Entry) -> Vec<(vault_store::LinkKey, vault_store::LinkRec)> {
    let mut m: HashMap<vault_store::LinkKey, vault_store::LinkRec> = HashMap::new();
    for l in &e.links {
        if let Some(d) = l.dst() {
            let r = m.entry((e.id, d, link_kind_code(l))).or_insert(vault_store::LinkRec { count: 0, offsets: Vec::new() });
            r.count += 1;
            r.offsets.push(l.start);
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by_key(|x| x.0);
    v
}
