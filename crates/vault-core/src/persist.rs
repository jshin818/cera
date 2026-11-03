//! Conversion between engine state and the on-disk startup snapshot (file table + links file).

use crate::intern::{Interner, NONE};
use crate::state::{Entry, OutLink, State};
use std::sync::Arc;
use vault_search::Frecency;
use vault_store::{LinksData, SnapDetail, SnapEntry, SnapHeading, SnapLink, SnapshotData};

pub fn to_snapshot(st: &State) -> (SnapshotData, LinksData) {
    let entries = st
        .entries
        .iter()
        .map(|e| {
            e.as_ref().map(|e| SnapEntry {
                path: e.path.clone(),
                markdown: e.markdown,
                size: e.size,
                mtime_ns: e.mtime_ns,
                hash: e.hash,
                title: e.title.clone(),
                aliases: e.aliases.clone(),
                tags: e.tags.clone(),
                word_count: e.word_count,
                tasks_open: e.tasks_open,
                tasks_done: e.tasks_done,
            })
        })
        .collect();
    let details = st
        .entries
        .iter()
        .map(|e| {
            e.as_ref().filter(|e| e.markdown).map(|e| SnapDetail {
                headings: e.headings.iter().map(|(l, t, o)| SnapHeading { level: *l, text: t.clone(), offset: *o }).collect(),
                links: e.links.iter().map(|l| SnapLink { target: l.target, sub: l.sub, display: l.display, start: l.start, end: l.end, dst: l.dst, flags: l.flags }).collect(),
                block_ids: e.block_ids.clone(),
                frontmatter_json: e.frontmatter.as_deref().unwrap_or_default().to_string(),
            })
        })
        .collect();
    (
        SnapshotData {
            generation: st.generation,
            entries,
            dirs: st.dirs.iter().cloned().collect(),
            pagerank: st.pagerank.clone(),
            clusters: st.clusters.clone(),
            frecency: st.frecency.iter().map(|f| (f.score, f.last_access_ms)).collect(),
        },
        LinksData { generation: st.generation, symbols: st.syms.to_vec(), details },
    )
}

/// Phase A: the file table only. Notes have no links yet; the sidebar and quick switcher work.
pub fn hydrate_core(d: SnapshotData) -> State {
    use rayon::prelude::*;
    let entries: Vec<Option<Arc<Entry>>> = d
        .entries
        .into_par_iter()
        .enumerate()
        .map(|(id, e)| {
            e.map(|e| {
                Arc::new(Entry {
                    id: id as u32,
                    path: e.path,
                    markdown: e.markdown,
                    size: e.size,
                    mtime_ns: e.mtime_ns,
                    hash: e.hash,
                    title: e.title,
                    aliases: e.aliases,
                    tags: e.tags,
                    headings: Vec::new(),
                    links: Vec::new(),
                    block_ids: Vec::new(),
                    word_count: e.word_count,
                    tasks_open: e.tasks_open,
                    tasks_done: e.tasks_done,
                    frontmatter: None,
                    backlinks: Vec::new(),
                })
            })
        })
        .collect();
    let mut st = State { generation: d.generation, ..Default::default() };
    let n = entries.len();
    {
        let by_path = Arc::make_mut(&mut st.by_path);
        by_path.reserve(n);
        for e in entries.iter().flatten() {
            by_path.insert(e.path.clone(), e.id);
            st.resolver.add_file(e.id, &e.path);
            for a in &e.aliases {
                st.resolver.add_alias(e.id, e.path.len() as u32, a);
            }
        }
    }
    st.entries = entries;
    st.dirs = d.dirs.into_iter().collect();
    st.pagerank = d.pagerank;
    st.clusters = d.clusters;
    st.frecency = d.frecency.into_iter().map(|(s, l)| Frecency { score: s, last_access_ms: l }).collect();
    st.frecency.resize(n, Frecency::default());
    st
}

/// Phase B: attach links, headings and frontmatter straight from the mapped (zero-copy) archive,
/// then derive backlinks, the unresolved map and target-name index.
pub fn attach_links(st: &mut State, a: &vault_store::snapshot::ArchivedLinksData) {
    use rayon::prelude::*;
    let syms: Vec<String> = a.symbols.iter().map(|s| s.as_str().to_string()).collect();
    st.syms = Arc::new(Interner::from_vec(syms));
    let n = st.entries.len().min(a.details.len());
    let rebuilt: Vec<Option<Arc<Entry>>> = (0..st.entries.len())
        .into_par_iter()
        .map(|i| {
            let e = st.entries[i].as_ref()?;
            let Some(d) = (i < n).then(|| a.details[i].as_ref()).flatten() else { return Some(e.clone()) };
            let mut ne = (**e).clone();
            ne.headings = d.headings.iter().map(|h| (h.level, h.text.as_str().to_string(), h.offset.to_native())).collect();
            ne.links = d
                .links
                .iter()
                .map(|l| OutLink {
                    target: l.target.to_native(),
                    lkey: NONE,
                    name: NONE,
                    sub: l.sub.to_native(),
                    display: l.display.to_native(),
                    start: l.start.to_native(),
                    end: l.end.to_native(),
                    dst: l.dst.to_native(),
                    flags: l.flags,
                })
                .collect();
            ne.block_ids = d.block_ids.iter().map(|b| b.as_str().to_string()).collect();
            ne.frontmatter = (!d.frontmatter_json.is_empty()).then(|| Arc::from(d.frontmatter_json.as_str()));
            Some(Arc::new(ne))
        })
        .collect();
    st.entries = rebuilt;
    // lkey / name symbols are derived (lower-cased targets), computed once per distinct target.
    let mut derived: std::collections::HashMap<u32, (u32, u32)> = std::collections::HashMap::new();
    let total = st.entries.len();
    let mut back: Vec<Vec<u32>> = vec![Vec::new(); total];
    for slot in st.entries.iter_mut() {
        let Some(e) = slot else { continue };
        if e.links.is_empty() {
            continue;
        }
        let em = Arc::make_mut(e);
        for l in em.links.iter_mut() {
            let (lkey, name) = *derived.entry(l.target).or_insert_with(|| {
                let t = st.syms.get(l.target);
                (st.syms.intern(&t.to_lowercase()), crate::state::target_name(&t).map(|x| st.syms.intern(&x)).unwrap_or(NONE))
            });
            l.lkey = lkey;
            l.name = name;
            match l.dst() {
                Some(d) if d != em.id && (d as usize) < total => back[d as usize].push(em.id),
                Some(_) => {}
                None => st.unresolved.entry(lkey).or_default().push(em.id),
            }
            if name != NONE {
                st.by_target_name.entry(name).or_default().push(em.id);
            }
        }
    }
    for v in st.unresolved.values_mut().chain(st.by_target_name.values_mut()) {
        v.sort_unstable();
        v.dedup();
    }
    for (slot, mut b) in st.entries.iter_mut().zip(back) {
        if let Some(e) = slot {
            if !b.is_empty() {
                b.sort_unstable();
                b.dedup();
                Arc::make_mut(e).backlinks = b;
            }
        }
    }
}
