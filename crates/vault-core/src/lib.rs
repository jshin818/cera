//! vault-core: the engine facade. Owns indexing, watching, search and discovery, and exposes
//! immutable snapshots. UI crates call methods here (from their own worker threads) and read
//! `Arc<Snapshot>`s; nothing here ever runs on a UI thread.
//!
//! Startup (the critical path, see `Engine::open`):
//! 1. mmap `.vault/snapshot.rkyv`  2. open redb + tantivy  3. caller renders the last note from disk
//! --- first frame --- then, on the indexer thread: hydrate, start watcher, parallel walk + diff,
//! embedding catch-up, graph metrics, persist snapshot on idle.

mod indexer;
mod ops;
mod persist;
mod query;
mod state;

pub use ops::{first_frame, safe_name, NoteView, SaveOutcome};
pub use query::*;
pub use state::{Entry, OutLink, Snapshot};
pub use vault_parse::Subpath;

use arc_swap::{ArcSwap, ArcSwapOption};
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: the file changed on disk since it was loaded")]
    Conflict { disk_hash: u64 },
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("already exists: {0}")]
    Exists(String),
    #[error("{0}")]
    Other(String),
}

macro_rules! other_err {
    ($($t:ty),*) => {$(
        impl From<$t> for CoreError {
            fn from(e: $t) -> Self { CoreError::Other(e.to_string()) }
        }
    )*};
}
other_err!(std::io::Error, vault_fs::FsError, vault_store::StoreError, vault_search::SearchError, vault_embed::EmbedError, serde_json::Error);

pub type Result<T> = std::result::Result<T, CoreError>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Install the file watcher (off for one-shot CLI commands and benchmarks).
    pub watch: bool,
    /// Run the background embedding worker.
    pub embed: bool,
    /// Where Model2Vec models live (`<dir>/potion-base-8M/model.safetensors`, ...).
    pub models_dir: Option<PathBuf>,
    pub debounce: Duration,
    pub commit_interval: Duration,
    /// Idle time before graph metrics / snapshot persistence run.
    pub idle: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            watch: true,
            embed: true,
            models_dir: default_models_dir(),
            debounce: Duration::from_millis(100),
            commit_interval: Duration::from_millis(250),
            idle: Duration::from_millis(1500),
        }
    }
}

pub fn default_models_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("VAULT_MODELS") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".vault-models"))
}

/// Startup and indexing timings (milliseconds), for `bench` and the status panel.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Timings {
    pub snapshot_map_ms: f64,
    pub snapshot_bytes: usize,
    pub stores_open_ms: f64,
    pub open_total_ms: f64,
    pub hydrate_ms: f64,
    pub walk_ms: f64,
    pub index_ms: f64,
    pub ready_ms: f64,
    pub files_reparsed: usize,
    pub embed_ms: f64,
    pub embed_chunks: usize,
    pub graph_ms: f64,
    pub snapshot_save_ms: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Status {
    pub ready: bool,
    pub generation: u64,
    pub notes: usize,
    pub attachments: usize,
    pub folders: usize,
    pub indexing: bool,
    pub embedding_pending: usize,
    pub embedded_files: usize,
    pub vectors: usize,
    pub model: String,
    pub clusters: usize,
    pub timings: Timings,
}

/// A change notification for subscribers (the UI shell).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangeEvent {
    pub generation: u64,
    pub paths: Vec<String>,
    /// True when the change came from outside the app (watcher), so open editors may reload.
    pub external: bool,
}

pub(crate) struct Inner {
    pub root: PathBuf,
    pub app_dir: PathBuf,
    pub cfg: vault_fs::ObsidianConfig,
    pub opts: EngineOptions,
    pub snap: ArcSwap<Snapshot>,
    pub state: Mutex<state::State>,
    pub store: vault_store::Store,
    pub text: vault_search::TextIndex,
    pub vectors: RwLock<vault_embed::VectorIndex>,
    pub embedder: RwLock<Arc<dyn vault_embed::Embedder>>,
    pub switcher: ArcSwapOption<vault_search::Switcher>,
    pub mentions: ArcSwapOption<vault_graph::MentionIndex>,
    pub neardup: Mutex<vault_graph::NearDupIndex>,
    pub minhasher: vault_graph::MinHasher,
    pub own_writes: Mutex<HashMap<String, u64>>,
    pub ready: (Mutex<bool>, Condvar),
    pub timings: Mutex<Timings>,
    pub events: tokio::sync::broadcast::Sender<ChangeEvent>,
    pub kick: crossbeam_channel::Sender<indexer::Cmd>,
    pub pending_commit: AtomicBool,
    pub dirty_graph: AtomicBool,
    pub dirty_titles: AtomicBool,
    pub dirty_persist: AtomicBool,
    pub dirty_vectors: AtomicBool,
    pub last_change: Mutex<Instant>,
    pub embed_tx: crossbeam_channel::Sender<(u32, Vec<vault_parse::Chunk>)>,
    pub embed_pending: AtomicU64,
    pub chunk_hashes: Mutex<HashMap<u32, Vec<u64>>>,
    pub related_cache: Mutex<HashMap<u32, (u64, u64, Vec<Related>)>>,
    pub vector_gen: AtomicU64,
    pub layout: Mutex<Option<vault_graph::Layout>>,
    pub tag_centroids: Mutex<Option<(u64, Vec<(String, Vec<f32>)>)>>,
    pub shutdown: AtomicBool,
    pub indexing: AtomicBool,
    pub threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// redb writes are applied on their own thread (redb is a rebuildable cache; nothing on the
    /// search/backlink visibility path waits for it).
    pub store_tx: Mutex<Option<crossbeam_channel::Sender<vault_store::StoreBatch>>>,
    pub store_pending: AtomicU64,
}

impl Inner {
    pub(crate) fn store_write(&self, b: vault_store::StoreBatch) {
        if b.is_empty() {
            return;
        }
        let tx = self.store_tx.lock();
        match tx.as_ref() {
            Some(tx) => {
                self.store_pending.fetch_add(1, Ordering::SeqCst);
                if tx.send(b).is_err() {
                    self.store_pending.fetch_sub(1, Ordering::SeqCst);
                }
            }
            // After shutdown: write synchronously.
            None => {
                let _ = self.store.apply(&b);
            }
        }
    }

    /// Wait until queued redb batches are written.
    pub(crate) fn store_drain(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.store_pending.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// The engine handle. Cheap to clone; all clones share one engine.
#[derive(Clone)]
pub struct Engine(pub(crate) Arc<Inner>);

impl Engine {
    /// Open a vault. Returns after the startup snapshot is mapped and the stores are open
    /// (the critical path); everything that scales with vault size continues on background threads.
    pub fn open(root: &Path, opts: EngineOptions) -> Result<Engine> {
        let t0 = Instant::now();
        let root = root.canonicalize().map_err(|e| CoreError::NotFound(format!("{}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(CoreError::Invalid(format!("{} is not a folder", root.display())));
        }
        let app_dir = root.join(vault_fs::APP_DIR);
        std::fs::create_dir_all(&app_dir)?;
        let cfg = vault_fs::ObsidianConfig::load(&root);

        let t = Instant::now();
        let mapped = vault_store::MappedSnapshot::open(&app_dir.join("snapshot.rkyv")).ok();
        let snapshot_map_ms = ms(t);
        let snapshot_bytes = mapped.as_ref().map(|m| m.len_bytes()).unwrap_or(0);

        let t = Instant::now();
        let store = match vault_store::Store::open(&app_dir.join("meta.redb")) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "meta.redb unreadable; rebuilding");
                let _ = std::fs::remove_file(app_dir.join("meta.redb"));
                vault_store::Store::open(&app_dir.join("meta.redb"))?
            }
        };
        let text = match vault_search::TextIndex::open(&app_dir.join("tantivy")) {
            Ok(t) => t,
            Err(e) if vault_search::TextIndex::is_lock_busy(&e) => {
                return Err(CoreError::Other(format!("this vault is already open in another Vault process ({e})")));
            }
            Err(e) => {
                tracing::warn!(error = %e, "tantivy index unreadable; rebuilding");
                let _ = std::fs::remove_dir_all(app_dir.join("tantivy"));
                vault_search::TextIndex::open(&app_dir.join("tantivy"))?
            }
        };
        let stores_open_ms = ms(t);

        let (kick_tx, kick_rx) = crossbeam_channel::unbounded();
        let (embed_tx, embed_rx) = crossbeam_channel::unbounded();
        let (events, _) = tokio::sync::broadcast::channel(256);
        let (store_tx, store_rx) = crossbeam_channel::unbounded::<vault_store::StoreBatch>();
        let embedder: Arc<dyn vault_embed::Embedder> = Arc::new(vault_embed::HashEmbedder);
        let inner = Arc::new(Inner {
            root,
            app_dir,
            cfg,
            opts,
            snap: ArcSwap::from_pointee(Snapshot::default()),
            state: Mutex::new(state::State::default()),
            store,
            text,
            vectors: RwLock::new(vault_embed::VectorIndex::new(embedder.name())),
            embedder: RwLock::new(embedder),
            switcher: ArcSwapOption::empty(),
            mentions: ArcSwapOption::empty(),
            neardup: Mutex::new(vault_graph::NearDupIndex::new()),
            minhasher: vault_graph::MinHasher::default(),
            own_writes: Mutex::new(HashMap::new()),
            ready: (Mutex::new(false), Condvar::new()),
            timings: Mutex::new(Timings { snapshot_map_ms, snapshot_bytes, stores_open_ms, open_total_ms: ms(t0), ..Default::default() }),
            events,
            kick: kick_tx,
            pending_commit: AtomicBool::new(false),
            dirty_graph: AtomicBool::new(false),
            dirty_titles: AtomicBool::new(false),
            dirty_persist: AtomicBool::new(false),
            dirty_vectors: AtomicBool::new(false),
            last_change: Mutex::new(Instant::now()),
            embed_tx,
            embed_pending: AtomicU64::new(0),
            chunk_hashes: Mutex::new(HashMap::new()),
            related_cache: Mutex::new(HashMap::new()),
            vector_gen: AtomicU64::new(0),
            layout: Mutex::new(None),
            tag_centroids: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            indexing: AtomicBool::new(true),
            threads: Mutex::new(Vec::new()),
            store_tx: Mutex::new(Some(store_tx)),
            store_pending: AtomicU64::new(0),
        });
        {
            let e = engine_weak(&inner);
            let h = std::thread::Builder::new()
                .name("vault-store".into())
                .spawn(move || {
                    while let Ok(b) = store_rx.recv() {
                        let Some(inner) = e.upgrade() else { break };
                        if let Err(err) = inner.store.apply(&b) {
                            tracing::error!(error = %err, "redb write failed");
                        }
                        inner.store_pending.fetch_sub(1, Ordering::SeqCst);
                    }
                })
                .expect("spawn store writer");
            inner.threads.lock().push(h);
        }
        let engine = Engine(inner);
        {
            let e = engine.clone();
            let h = std::thread::Builder::new().name("vault-indexer".into()).spawn(move || indexer::run(e, mapped, kick_rx)).expect("spawn indexer");
            engine.0.threads.lock().push(h);
        }
        if engine.0.opts.embed {
            let e = engine.clone();
            let h = std::thread::Builder::new().name("vault-embed".into()).spawn(move || indexer::embed_worker(e, embed_rx)).expect("spawn embedder");
            engine.0.threads.lock().push(h);
        }
        Ok(engine)
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }

    /// Block until the first walk + index pass has finished (or timeout). Returns readiness.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let (m, cv) = &self.0.ready;
        let mut r = m.lock();
        let deadline = Instant::now() + timeout;
        while !*r {
            if cv.wait_until(&mut r, deadline).timed_out() {
                break;
            }
        }
        *r
    }

    pub fn is_ready(&self) -> bool {
        *self.0.ready.0.lock()
    }

    /// Block until queued embeddings are done (or timeout).
    pub fn wait_embeddings(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.is_ready() && self.0.embed_pending.load(Ordering::SeqCst) == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Wait for pending text-index commits to become visible to search.
    pub fn flush(&self) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.0.kick.send(indexer::Cmd::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(30));
        }
        self.0.store_drain(Duration::from_secs(60));
    }

    /// Lightweight size+mtime verify of the whole vault (on window focus, or after FSEvents
    /// reported a rescan). Runs on the indexer thread.
    pub fn rescan(&self) {
        let _ = self.0.kick.send(indexer::Cmd::Rescan);
    }

    /// Recompute graph metrics now (normally done on idle).
    pub fn recompute_graph(&self) {
        indexer::compute_graph(self);
    }

    /// Persist the startup snapshot and vectors now (normally done on idle and on drop).
    pub fn persist(&self) -> Result<()> {
        indexer::persist(self)
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.0.snap.load_full()
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ChangeEvent> {
        self.0.events.subscribe()
    }

    pub fn timings(&self) -> Timings {
        self.0.timings.lock().clone()
    }

    pub fn status(&self) -> Status {
        let s = self.snapshot();
        let v = self.0.vectors.read();
        Status {
            ready: self.is_ready(),
            generation: s.generation,
            notes: s.notes,
            attachments: s.attachments,
            folders: s.dirs.len(),
            indexing: self.0.indexing.load(Ordering::Relaxed),
            embedding_pending: self.0.embed_pending.load(Ordering::Relaxed) as usize,
            embedded_files: v.files().count(),
            vectors: v.len(),
            model: self.0.embedder.read().name().to_string(),
            clusters: s.clusters.iter().filter(|&&c| c != u32::MAX).max().map(|m| *m as usize + 1).unwrap_or(0),
            timings: self.timings(),
        }
    }

    /// Stop background threads, persist caches and release the index lock. Call from a non-engine
    /// thread; the engine must not be used afterwards.
    pub fn shutdown(&self) {
        if self.0.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.0.kick.send(indexer::Cmd::Shutdown);
        let me = std::thread::current().id();
        let handles: Vec<_> = self.0.threads.lock().drain(..).collect();
        let (store_h, others): (Vec<_>, Vec<_>) = handles.into_iter().partition(|h| h.thread().name() == Some("vault-store"));
        for h in others {
            if h.thread().id() != me {
                let _ = h.join();
            }
        }
        if self.is_ready() {
            let _ = self.persist();
        }
        // Close the store queue; the writer drains it and exits.
        self.0.store_tx.lock().take();
        for h in store_h {
            let _ = h.join();
        }
        let _ = self.0.text.close();
    }
}

fn engine_weak(inner: &Arc<Inner>) -> std::sync::Weak<Inner> {
    Arc::downgrade(inner)
}

pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
