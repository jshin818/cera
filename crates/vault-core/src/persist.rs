//! Conversion between engine state and the on-disk startup snapshot.

use crate::state::{Entry, OutLink, State};
use std::sync::Arc;
use vault_parse::Subpath;
use vault_search::Frecency;
use vault_store::{SnapEntry, SnapHeading, SnapLink, SnapshotData};

pub fn to_snapshot(st: &State) -> SnapshotData {
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
                headings: e.headings.iter().map(|(l, t, o)| SnapHeading { level: *l, text: t.clone(), offset: *o }).collect(),
                links: e
                    .links
                    .iter()
                    .map(|l| {
                        let (sub_kind, subpath) = match &l.subpath {
                            None => (0, String::new()),
                            Some(Subpath::Heading(h)) => (1, h.clone()),
                            Some(Subpath::Block(b)) => (2, b.clone()),
                        };
                        SnapLink {
                            target: l.target.clone(),
                            sub_kind,
                            subpath,
                            display: l.display.clone().unwrap_or_default(),
                            kind: l.embed as u8,
                            start: l.start,
                            end: l.end,
                            dst: l.dst.unwrap_or(u32::MAX),
                        }
                    })
                    .collect(),
                block_ids: e.block_ids.clone(),
                word_count: e.word_count,
                tasks_open: e.tasks_open,
                tasks_done: e.tasks_done,
                frontmatter_json: e.frontmatter.as_ref().map(|f| f.to_string()).unwrap_or_default(),
            })
        })
        .collect();
    SnapshotData {
        generation: st.generation,
        entries,
        dirs: st.dirs.iter().cloned().collect(),
        pagerank: st.pagerank.clone(),
        clusters: st.clusters.clone(),
        frecency: st.frecency.iter().map(|f| (f.score, f.last_access_ms)).collect(),
    }
}

/// Rebuild engine state from a snapshot without reparsing any file. Backlinks, the resolver and
/// the unresolved map are derived here.
pub fn from_snapshot(d: SnapshotData) -> State {
    use rayon::prelude::*;
    let entries: Vec<Option<Entry>> = d
        .entries
        .into_par_iter()
        .enumerate()
        .map(|(id, e)| {
            e.map(|e| Entry {
                id: id as u32,
                path: e.path,
                markdown: e.markdown,
                size: e.size,
                mtime_ns: e.mtime_ns,
                hash: e.hash,
                title: e.title,
                aliases: e.aliases,
                tags: e.tags,
                headings: e.headings.into_iter().map(|h| (h.level, h.text, h.offset)).collect(),
                links: e
                    .links
                    .into_iter()
                    .map(|l| OutLink {
                        target: l.target,
                        subpath: match l.sub_kind {
                            1 => Some(Subpath::Heading(l.subpath)),
                            2 => Some(Subpath::Block(l.subpath)),
                            _ => None,
                        },
                        display: (!l.display.is_empty()).then_some(l.display),
                        embed: l.kind == 1,
                        start: l.start,
                        end: l.end,
                        dst: (l.dst != u32::MAX).then_some(l.dst),
                    })
                    .collect(),
                block_ids: e.block_ids,
                word_count: e.word_count,
                tasks_open: e.tasks_open,
                tasks_done: e.tasks_done,
                frontmatter: (!e.frontmatter_json.is_empty()).then(|| serde_json::from_str(&e.frontmatter_json).ok()).flatten(),
                backlinks: Vec::new(),
            })
        })
        .collect();
    let mut st = State { generation: d.generation, ..Default::default() };
    let n = entries.len();
    let mut back: Vec<Vec<u32>> = vec![Vec::new(); n];
    for e in entries.iter().flatten() {
        st.by_path.insert(e.path.clone(), e.id);
        st.resolver.add_file(e.id, &e.path);
        for a in &e.aliases {
            st.resolver.add_alias(e.id, e.path.len() as u32, a);
        }
        for l in &e.links {
            match l.dst {
                Some(dd) if dd != e.id && (dd as usize) < n => back[dd as usize].push(e.id),
                Some(_) => {}
                None => st.unresolved.entry(l.target.to_lowercase()).or_default().push(e.id),
            }
            if let Some(name) = crate::state::target_name(&l.target) {
                st.by_target_name.entry(name).or_default().insert(e.id);
            }
        }
    }
    for v in st.unresolved.values_mut() {
        v.sort_unstable();
        v.dedup();
    }
    st.entries = entries
        .into_iter()
        .zip(back)
        .map(|(e, mut b)| {
            e.map(|mut e| {
                b.sort_unstable();
                b.dedup();
                e.backlinks = b;
                Arc::new(e)
            })
        })
        .collect();
    st.dirs = d.dirs.into_iter().collect();
    st.pagerank = d.pagerank;
    st.clusters = d.clusters;
    st.frecency = d.frecency.into_iter().map(|(s, l)| Frecency { score: s, last_access_ms: l }).collect();
    st.frecency.resize(n, Frecency::default());
    st
}
