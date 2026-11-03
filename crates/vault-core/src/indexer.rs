//! Background work: hydrate, walk + diff, watcher loop with batched commits, embedding worker,
//! graph metrics and persistence. Everything here runs off the UI thread.

use crate::state::{FileUpdate, State};
use crate::{ms, ChangeEvent, Engine, Inner, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use rayon::prelude::*;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vault_fs::{FileKind, WatchEvent};

pub enum Cmd {
    Kick,
    Flush(crossbeam_channel::Sender<()>),
    Rescan,
    Shutdown,
}

pub fn run(engine: Engine, mapped: Option<vault_store::MappedSnapshot>, links: Option<vault_store::MappedLinks>, kick: Receiver<Cmd>) {
    let inner = &engine.0;
    let t_ready = Instant::now();

    // Hydrate from the snapshot (no file reads). Phase A: the file table, published at once so
    // the sidebar and quick switcher work; phase B: links, headings, backlinks.
    let t = Instant::now();
    let mut force_text_rebuild = false;
    let mut force_reparse = false;
    if let Some(m) = mapped {
        match m.to_owned() {
            Ok(data) => {
                let st = crate::persist::hydrate_core(data);
                let notes = st.entries.iter().flatten().filter(|e| e.markdown).count() as u64;
                if inner.text.num_docs() != notes {
                    tracing::info!(index = inner.text.num_docs(), notes, "text index out of sync with snapshot; rebuilding");
                    force_text_rebuild = true;
                }
                let mut g = inner.state.lock();
                *g = st;
                publish(inner, &g);
            }
            Err(e) => tracing::warn!(error = %e, "snapshot decode failed; full reindex"),
        }
        drop(m);
        inner.timings.lock().hydrate_core_ms = ms(t);
        rebuild_switcher(inner);
        let mut ok = false;
        if let Some(mut l) = links {
            if let Ok(a) = l.archived() {
                let mut st = std::mem::take(&mut *inner.state.lock());
                if a.generation.to_native() == st.generation {
                    crate::persist::attach_links(&mut st, a);
                    ok = true;
                }
                let mut g = inner.state.lock();
                *g = st;
                publish(inner, &g);
            }
        }
        if !ok {
            tracing::info!("links file missing or stale; reparsing notes");
            force_reparse = true;
        }
    }
    inner.timings.lock().hydrate_ms = ms(t);
    if inner.shutdown.load(Ordering::SeqCst) {
        return;
    }

    // Watcher before the walk finishes, so nothing slips between walk and watch.
    let watcher = if inner.opts.watch {
        match vault_fs::Watcher::start(&inner.root, inner.cfg.clone(), inner.opts.debounce) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "file watcher unavailable; relying on rescans");
                None
            }
        }
    } else {
        None
    };

    // Load vectors (or start fresh if the model changed).
    load_vectors(inner);

    let t = Instant::now();
    let files = vault_fs::walk(&inner.root, &inner.cfg);
    let dirs = vault_fs::walk_dirs(&inner.root, &inner.cfg);
    inner.timings.lock().walk_ms = ms(t);

    let t = Instant::now();
    if force_text_rebuild || force_reparse {
        if force_text_rebuild {
            let _ = inner.text.clear();
        }
        inner.state.lock().entries.iter_mut().for_each(|e| {
            if let Some(x) = e {
                // Force a reparse by forgetting the hash.
                let mut ne = (**x).clone();
                ne.hash = 0;
                ne.size = u64::MAX;
                *x = Arc::new(ne);
            }
        });
    }
    let reparsed = sync_with_disk(&engine, files, dirs);
    let tc = Instant::now();
    let commit_res = inner.text.commit();
    tracing::info!(commit_ms = ms(tc), "initial commit");
    if let Err(e) = commit_res {
        tracing::error!(error = %e, "tantivy commit failed");
    }
    {
        let mut tm = inner.timings.lock();
        tm.index_ms = ms(t);
        tm.files_reparsed = reparsed;
        tm.ready_ms = ms(t_ready);
    }
    if reparsed > 0 || force_reparse {
        rebuild_switcher(inner);
    }
    inner.indexing.store(false, Ordering::SeqCst);
    {
        let (m, cv) = &inner.ready;
        *m.lock() = true;
        cv.notify_all();
    }
    // Embedding catch-up for notes without vectors (after the text index: never competes with it).
    if inner.opts.embed {
        enqueue_missing_embeddings(inner);
    }
    inner.dirty_graph.store(true, Ordering::SeqCst);
    inner.dirty_titles.store(true, Ordering::SeqCst);
    {
        let e = engine.clone();
        let h = std::thread::Builder::new().name("vault-bg".into()).spawn(move || background(e)).expect("spawn bg");
        inner.threads.lock().push(h);
    }

    let mut last_commit = Instant::now();
    let empty = crossbeam_channel::never();
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let wrx = watcher.as_ref().map(|w| &w.events).unwrap_or(&empty);
        let mut changed: Vec<String> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        let mut rescan = false;
        crossbeam_channel::select! {
            recv(wrx) -> ev => match ev {
                Ok(WatchEvent::Changed(p)) => changed.extend(p),
                Ok(WatchEvent::Removed(p)) => removed.extend(p),
                Ok(WatchEvent::Rescan) => rescan = true,
                Err(_) => {}
            },
            recv(kick) -> c => match c {
                Ok(Cmd::Kick) => {}
                Ok(Cmd::Rescan) => rescan = true,
                Ok(Cmd::Flush(done)) => {
                    let _ = inner.text.commit();
                    inner.pending_commit.store(false, Ordering::SeqCst);
                    last_commit = Instant::now();
                    let _ = done.send(());
                }
                Ok(Cmd::Shutdown) | Err(_) => break,
            },
            default(if inner.pending_commit.load(Ordering::SeqCst) { Duration::from_millis(10) } else { Duration::from_millis(100) }) => {}
        }
        // Drain whatever else arrived to batch it.
        if let Some(w) = &watcher {
            while let Ok(ev) = w.events.try_recv() {
                match ev {
                    WatchEvent::Changed(p) => changed.extend(p),
                    WatchEvent::Removed(p) => removed.extend(p),
                    WatchEvent::Rescan => rescan = true,
                }
            }
        }
        if rescan {
            let files = vault_fs::walk(&inner.root, &inner.cfg);
            let dirs = vault_fs::walk_dirs(&inner.root, &inner.cfg);
            sync_with_disk(&engine, files, dirs);
        } else if !changed.is_empty() || !removed.is_empty() {
            // Directory events: expand to their contents.
            let mut paths: Vec<String> = changed.into_iter().chain(removed).collect();
            paths.sort();
            paths.dedup();
            let t = Instant::now();
            let n = paths.len();
            process_paths(&engine, paths, true);
            tracing::debug!(paths = n, ms = ms(t), "watch batch");
        }
        // Commit policy: an isolated change commits after a 30 ms quiet window; a burst of changes
        // commits at most every `commit_interval` (~250 ms) so typing never floods tantivy.
        let quiet = inner.last_change.lock().elapsed();
        if inner.pending_commit.load(Ordering::SeqCst) && (last_commit.elapsed() >= inner.opts.commit_interval || (quiet >= Duration::from_millis(30) && last_commit.elapsed() >= Duration::from_millis(60))) {
            inner.pending_commit.store(false, Ordering::SeqCst);
            let t = Instant::now();
            if let Err(e) = inner.text.commit() {
                tracing::error!(error = %e, "tantivy commit failed");
            }
            tracing::debug!(ms = ms(t), "commit");
            last_commit = Instant::now();
        }
    }
}

/// Low-priority background loop: switcher/mention rebuilds, graph metrics and persistence. Kept off
/// the indexer thread so watcher events and commits never wait behind multi-second jobs.
fn background(engine: Engine) {
    let inner = &engine.0;
    lower_priority();
    while !inner.shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        let quiet = inner.last_change.lock().elapsed();
        if inner.dirty_titles.load(Ordering::SeqCst) && quiet > Duration::from_millis(150) {
            inner.dirty_titles.store(false, Ordering::SeqCst);
            rebuild_switcher(inner);
            inner.mentions.store(None); // rebuilt lazily on next use
        }
        if quiet < inner.opts.idle {
            continue;
        }
        if inner.dirty_graph.load(Ordering::SeqCst) && inner.embed_pending.load(Ordering::SeqCst) == 0 {
            compute_graph(&engine);
        }
        if inner.dirty_persist.load(Ordering::SeqCst) && inner.embed_pending.load(Ordering::SeqCst) == 0 && inner.last_change.lock().elapsed() >= inner.opts.idle {
            if let Err(e) = persist(&engine) {
                tracing::warn!(error = %e, "persist failed");
            }
        }
    }
}

/// Run the current thread at background priority (macOS: utility QoS, which favours efficiency
/// cores; Linux: nice 10), so indexing never competes with interactive queries.
pub(crate) fn lower_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
        }
        pthread_set_qos_class_self_np(0x11, 0);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        unsafe extern "C" {
            fn syscall(n: i64, ...) -> i64;
            fn setpriority(which: i32, who: u32, prio: i32) -> i32;
        }
        let tid = syscall(186) as u32; // SYS_gettid on x86_64
        #[cfg(target_arch = "aarch64")]
        let tid = syscall(178) as u32;
        setpriority(0, tid, 10);
    }
}

/// Compare the walk with the current state and process differences. Returns files reparsed.
fn sync_with_disk(engine: &Engine, files: Vec<vault_fs::WalkedFile>, dirs: Vec<String>) -> usize {
    let inner = &engine.0;
    let (changed, removed): (Vec<vault_fs::WalkedFile>, Vec<String>) = {
        let st = inner.state.lock();
        let on_disk: HashSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        let removed = st.by_path.keys().filter(|p| !on_disk.contains(p.as_str())).cloned().collect();
        let changed = files
            .into_iter()
            .filter(|f| match st.by_path.get(&f.rel).and_then(|&id| st.entries[id as usize].as_ref()) {
                Some(e) => e.size != f.size || e.mtime_ns != f.mtime_ns,
                None => true,
            })
            .collect();
        (changed, removed)
    };
    {
        let mut st = inner.state.lock();
        let before = st.dirs.len();
        st.dirs = dirs.into_iter().collect();
        if st.dirs.len() != before {
            st.generation += 1;
            publish(inner, &st);
        }
    }
    let n = changed.len();
    {
        let new_paths: Vec<&str> = changed.iter().map(|f| f.rel.as_str()).collect();
        inner.state.lock().preregister(&new_paths);
    }
    // Bounded batches keep peak memory flat on the first index of a large vault.
    let mut reparsed = 0;
    let mut first = true;
    for batch in changed.chunks(4000) {
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let t = Instant::now();
        let updates = read_and_parse(inner, batch, false);
        let t_parse = ms(t);
        reparsed += updates.iter().filter(|u| u.parsed.is_some()).count();
        let t = Instant::now();
        apply(engine, updates, if first { removed.clone() } else { Vec::new() }, false);
        tracing::info!(files = batch.len(), parse_ms = t_parse, apply_ms = ms(t), "index batch");
        first = false;
    }
    if first && !removed.is_empty() {
        apply(engine, Vec::new(), removed, false);
    }
    tracing::debug!(changed = n, reparsed, "sync with disk");
    reparsed
}

/// Watcher path: stat each path and treat missing ones as removals; skip the app's own writes.
pub(crate) fn process_paths(engine: &Engine, paths: Vec<String>, external: bool) {
    let inner = &engine.0;
    let mut files = Vec::new();
    let mut removed = Vec::new();
    let mut dir_changed = false;
    for p in paths {
        let abs = vault_fs::to_abs(&inner.root, &p);
        match std::fs::metadata(&abs) {
            Ok(m) if m.is_file() => files.push(vault_fs::WalkedFile { size: m.len(), mtime_ns: vault_fs::mtime_ns(&m), kind: FileKind::of(&p), rel: p }),
            Ok(m) if m.is_dir() => {
                dir_changed = true;
                // A folder appeared (e.g. moved in): index its contents.
                for f in vault_fs::walk(&abs, &inner.cfg) {
                    let rel = format!("{p}/{}", f.rel);
                    files.push(vault_fs::WalkedFile { rel, ..f });
                }
            }
            Ok(_) => {}
            Err(_) => {
                // Gone: a file, or a folder whose files we must drop.
                let st = inner.state.lock();
                if st.by_path.contains_key(&p) {
                    removed.push(p);
                } else {
                    let prefix = format!("{p}/");
                    removed.extend(st.by_path.keys().filter(|k| k.starts_with(&prefix)).cloned());
                    dir_changed |= st.dirs.contains(&p);
                }
            }
        }
    }
    if dir_changed {
        let dirs = vault_fs::walk_dirs(&inner.root, &inner.cfg);
        let mut st = inner.state.lock();
        st.dirs = dirs.into_iter().collect();
        st.generation += 1;
        publish(inner, &st);
    }
    // Skip unchanged stats.
    {
        let st = inner.state.lock();
        files.retain(|f| match st.by_path.get(&f.rel).and_then(|&id| st.entries[id as usize].as_ref()) {
            Some(e) => e.size != f.size || e.mtime_ns != f.mtime_ns,
            None => true,
        });
    }
    if files.is_empty() && removed.is_empty() {
        return;
    }
    let updates = read_and_parse(inner, &files, true);
    apply(engine, updates, removed, external);
}

/// Read, hash and parse files in parallel. With `skip_own`, files whose hash matches the app's
/// last write are dropped (the write path already applied them).
fn read_and_parse(inner: &Inner, files: &[vault_fs::WalkedFile], skip_own: bool) -> Vec<FileUpdate> {
    let known: Vec<Option<u64>> = {
        let st = inner.state.lock();
        files.iter().map(|f| st.by_path.get(&f.rel).and_then(|&id| st.entries[id as usize].as_ref()).map(|e| e.hash)).collect()
    };
    let own = inner.own_writes.lock().clone();
    let out: Vec<Option<FileUpdate>> = files
        .par_iter()
        .zip(known)
        .map(|(f, known_hash)| {
            let bytes = vault_fs::read(&inner.root, &f.rel).ok()?;
            let hash = vault_fs::hash_bytes(&bytes);
            if skip_own && own.get(&f.rel) == Some(&hash) && known_hash == Some(hash) {
                return None;
            }
            if known_hash == Some(hash) || f.kind != FileKind::Markdown {
                return Some(FileUpdate { rel: f.rel.clone(), size: f.size, mtime_ns: f.mtime_ns, hash, parsed: None, chunks: Vec::new(), minhash: None });
            }
            let src = String::from_utf8_lossy(&bytes);
            Some(parse_update(inner, &f.rel, f.size, f.mtime_ns, hash, &src))
        })
        .collect();
    out.into_iter().flatten().collect()
}

pub(crate) fn parse_update(inner: &Inner, rel: &str, size: u64, mtime_ns: i64, hash: u64, src: &str) -> FileUpdate {
    let parsed = vault_parse::parse(rel, src);
    let chunks = Vec::new(); // the embedding worker re-chunks from disk when it gets to the file
    let _ = inner;
    let minhash = inner.minhasher.signature(&parsed.body_text);
    FileUpdate { rel: rel.to_string(), size, mtime_ns, hash, parsed: Some(parsed), chunks, minhash }
}

/// Apply updates under the state lock, stage index/store writes, publish a new snapshot.
pub(crate) fn apply(engine: &Engine, updates: Vec<FileUpdate>, removals: Vec<String>, external: bool) -> u64 {
    let inner = &engine.0;
    let t0 = Instant::now();
    let mut st = inner.state.lock();
    let applied = st.apply(updates, removals);
    let t_state = ms(t0);
    if applied.changed_paths.is_empty() {
        return st.generation;
    }
    for id in &applied.removed_ids {
        inner.text.delete(*id);
    }
    let t_text = Instant::now();
    if let Err(e) = inner.text.upsert_many(&applied.index_docs, true) {
        tracing::error!(error = %e, "index upsert failed");
    }
    let t_text = ms(t_text);
    let t_store = Instant::now();
    inner.store_write(applied.store);
    let t_store = ms(t_store);
    {
        let mut nd = inner.neardup.lock();
        for (id, sig) in &applied.minhash {
            match sig {
                Some(s) => nd.insert(*id, *s),
                None => nd.remove(*id),
            }
        }
    }
    if !applied.removed_ids.is_empty() {
        let mut v = inner.vectors.write();
        for id in &applied.removed_ids {
            v.remove_file(*id);
        }
        inner.vector_gen.fetch_add(1, Ordering::SeqCst);
    }
    let t_pub = Instant::now();
    publish(inner, &st);
    tracing::debug!(state_ms = t_state, text_ms = t_text, store_ms = t_store, publish_ms = ms(t_pub), total_ms = ms(t0), "apply");
    let generation = st.generation;
    drop(st);
    // During the first index the catch-up pass embeds everything afterwards; later edits embed now.
    if inner.opts.embed && !inner.indexing.load(Ordering::SeqCst) {
        for (id, _) in applied.embed {
            inner.embed_pending.fetch_add(1, Ordering::SeqCst);
            let _ = inner.embed_tx.send(id);
        }
    }
    inner.pending_commit.store(true, Ordering::SeqCst);
    if applied.titles_changed {
        inner.dirty_titles.store(true, Ordering::SeqCst);
    }
    if applied.graph_changed {
        inner.dirty_graph.store(true, Ordering::SeqCst);
    }
    inner.dirty_persist.store(true, Ordering::SeqCst);
    *inner.last_change.lock() = Instant::now();
    inner.related_cache.lock().clear();
    let _ = inner.events.send(ChangeEvent { generation, paths: applied.changed_paths, external });
    let _ = inner.kick.send(Cmd::Kick);
    generation
}

pub(crate) fn publish(inner: &Inner, st: &State) {
    inner.snap.store(Arc::new(st.snapshot()));
}

pub(crate) fn rebuild_switcher(inner: &Inner) {
    let snap = inner.snap.load_full();
    let now = crate::now_ms();
    let items: Vec<vault_search::SwitchItem> = snap
        .notes()
        .map(|e| vault_search::SwitchItem { id: e.id, title: e.title.clone(), aliases: e.aliases.clone(), path: e.path.clone() })
        .collect();
    let s2 = snap.clone();
    let s3 = snap.clone();
    let sw = vault_search::Switcher::new(items, move |id| s2.frecency_norm(id, now), move |id| s3.pagerank_norm(id));
    inner.switcher.store(Some(Arc::new(sw)));
}

pub(crate) fn mentions(inner: &Inner) -> Arc<vault_graph::MentionIndex> {
    if let Some(m) = inner.mentions.load_full() {
        return m;
    }
    let snap = inner.snap.load_full();
    let names = snap.notes().flat_map(|e| std::iter::once((e.id, e.title.clone())).chain(e.aliases.iter().map(move |a| (e.id, a.clone()))));
    let m = Arc::new(vault_graph::MentionIndex::new(names.collect::<Vec<_>>(), 3));
    inner.mentions.store(Some(m.clone()));
    m
}

/// Global PageRank + Leiden clusters on the current link graph.
pub(crate) fn compute_graph(engine: &Engine) {
    let inner = &engine.0;
    inner.dirty_graph.store(false, Ordering::SeqCst);
    let t = Instant::now();
    let (g, active) = inner.state.lock().graph();
    let (pr, clusters) = rayon::join(|| vault_graph::pagerank(&g, &active, 0.85, 30, 1e-7), || vault_graph::leiden(&g, 1.0, 8));
    let clusters: Vec<u32> = clusters.into_iter().zip(&active).map(|(c, a)| if *a { c } else { u32::MAX }).collect();
    let mut st = inner.state.lock();
    if st.entries.len() == pr.len() {
        let batch = vault_store::StoreBatch {
            graph: (0..pr.len()).filter(|&i| active[i]).map(|i| (i as u32, vault_store::GraphRec { pagerank: pr[i], cluster_id: clusters[i] })).collect(),
            ..Default::default()
        };
        st.pagerank = pr;
        st.clusters = clusters;
        st.generation += 1;
        publish(inner, &st);
        drop(st);
        inner.store_write(batch);
        inner.dirty_persist.store(true, Ordering::SeqCst);
        rebuild_switcher(inner);
    } else {
        // State grew meanwhile; try again on the next idle tick.
        inner.dirty_graph.store(true, Ordering::SeqCst);
    }
    inner.timings.lock().graph_ms = ms(t);
}

pub(crate) fn persist(engine: &Engine) -> Result<()> {
    let inner = &engine.0;
    let _guard = inner.persist_lock.lock();
    inner.dirty_persist.store(false, Ordering::SeqCst);
    let t = Instant::now();
    inner.store_drain(Duration::from_secs(60));
    let (data, links) = crate::persist::to_snapshot(&inner.state.lock());
    vault_store::snapshot::save(&inner.app_dir, &data, &links)?;
    if inner.dirty_vectors.swap(false, Ordering::SeqCst) {
        inner.vectors.write().save(&inner.app_dir.join("vectors").join("index.bin"))?;
    }
    if let Some(l) = inner.layout.lock().as_ref() {
        let _ = l.save(&inner.app_dir.join("layout.bin"));
    }
    inner.timings.lock().snapshot_save_ms = ms(t);
    Ok(())
}

fn load_vectors(inner: &Inner) {
    let emb = vault_embed::best_available(inner.opts.models_dir.as_deref());
    let name = emb.name().to_string();
    *inner.embedder.write() = Arc::from(emb);
    let path = inner.app_dir.join("vectors").join("index.bin");
    match vault_embed::VectorIndex::load(&path) {
        Ok(v) if v.model == name => *inner.vectors.write() = v,
        Ok(v) => {
            tracing::info!(old = %v.model, new = %name, "embedding model changed; re-embedding");
            *inner.vectors.write() = vault_embed::VectorIndex::new(&name);
        }
        Err(_) => *inner.vectors.write() = vault_embed::VectorIndex::new(&name),
    }
    // Drop vectors of files that no longer exist.
    let snap = inner.snap.load_full();
    let mut v = inner.vectors.write();
    let stale: Vec<u32> = v.files().filter(|id| snap.get(*id).is_none_or(|e| !e.markdown)).collect();
    for id in stale {
        v.remove_file(id);
    }
}

fn enqueue_missing_embeddings(inner: &Inner) {
    let snap = inner.snap.load_full();
    let missing: Vec<u32> = {
        let v = inner.vectors.read();
        snap.notes().filter(|e| !v.has_file(e.id)).map(|e| e.id).collect()
    };
    inner.embed_pending.fetch_add(missing.len() as u64, Ordering::SeqCst);
    for id in missing {
        let _ = inner.embed_tx.send(id);
    }
}

/// Embedding worker: drains file ids in batches, chunks them from disk and re-embeds only chunks
/// whose XXH3 changed. Runs at background priority.
pub fn embed_worker(engine: Engine, rx: Receiver<u32>) {
    let inner = &engine.0;
    lower_priority();
    let mut total_ms = 0.0;
    let mut total_chunks = 0usize;
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(x) => x,
            Err(RecvTimeoutError::Timeout) => {
                if inner.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if inner.shutdown.load(Ordering::SeqCst) {
            return;
        }
        let mut ids = vec![first];
        while ids.len() < 512 {
            match rx.try_recv() {
                Ok(x) => ids.push(x),
                Err(_) => break,
            }
        }
        let n_ids = ids.len() as u64;
        let t = Instant::now();
        let emb = inner.embedder.read().clone();
        let snap = inner.snap.load_full();
        ids.sort_unstable();
        ids.dedup();
        let batch: Vec<(u32, Vec<vault_parse::Chunk>)> = ids
            .par_iter()
            .filter_map(|&id| {
                let e = snap.get(id).filter(|e| e.markdown)?;
                let src = vault_fs::read(&inner.root, &e.path).ok()?;
                let src = String::from_utf8_lossy(&src);
                let p = vault_parse::parse(&e.path, &src);
                Some((id, vault_parse::chunk_note(&src, &p)))
            })
            .collect();
        enum Plan {
            New(usize),
            Reuse(Vec<f32>),
        }
        let mut texts: Vec<&str> = Vec::new();
        let mut plan: Vec<Vec<Plan>> = Vec::with_capacity(batch.len());
        {
            let old_hashes = inner.chunk_hashes.lock();
            let v = inner.vectors.read();
            for (id, chunks) in &batch {
                let prev = old_hashes.get(id);
                let prev_vecs = prev.and_then(|_| v.file_vector_rows(*id));
                let mut p = Vec::with_capacity(chunks.len());
                for c in chunks {
                    let reuse = prev.zip(prev_vecs.as_ref()).and_then(|(h, vecs)| h.iter().position(|x| *x == c.hash).and_then(|j| vecs.get(j).cloned()));
                    match reuse {
                        Some(vec) => p.push(Plan::Reuse(vec)),
                        None => {
                            p.push(Plan::New(texts.len()));
                            texts.push(&c.text);
                        }
                    }
                }
                plan.push(p);
            }
        }
        let fresh = emb.embed(&texts);
        let n_new = texts.len();
        {
            let mut v = inner.vectors.write();
            if v.model != emb.name() {
                *v = vault_embed::VectorIndex::new(emb.name());
            }
            let mut hashes = inner.chunk_hashes.lock();
            let mut store = vault_store::StoreBatch::default();
            for (p, (id, chunks)) in plan.into_iter().zip(batch.iter()) {
                let vecs: Vec<Vec<f32>> = p
                    .into_iter()
                    .map(|r| match r {
                        Plan::New(i) => fresh[i].clone(),
                        Plan::Reuse(x) => x,
                    })
                    .collect();
                v.upsert_file(*id, &vecs);
                hashes.insert(*id, chunks.iter().map(|c| c.hash).collect());
                store.chunks.push((
                    *id,
                    chunks.iter().map(|c| vault_store::ChunkRec { file_id: *id, start: c.start, end: c.end, heading_path: c.heading_path.clone(), model: emb.name().to_string(), xxh3: c.hash }).collect(),
                ));
            }
            inner.store_write(store);
        }
        inner.vector_gen.fetch_add(1, Ordering::SeqCst);
        inner.dirty_vectors.store(true, Ordering::SeqCst);
        inner.dirty_persist.store(true, Ordering::SeqCst);
        inner.related_cache.lock().clear();
        total_ms += ms(t);
        total_chunks += n_new;
        {
            let mut tm = inner.timings.lock();
            tm.embed_ms = total_ms;
            tm.embed_chunks = total_chunks;
        }
        inner.embed_pending.fetch_sub(n_ids, Ordering::SeqCst);
    }
}
