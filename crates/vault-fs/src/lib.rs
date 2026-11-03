//! File-system layer: parallel walk, change detection, FSEvents/inotify watching and atomic writes.
//!
//! Paths inside the engine are always vault-relative, `/`-separated strings ("normalized paths").

mod obsidian;
mod watch;
mod write;

pub use obsidian::ObsidianConfig;
pub use watch::{WatchEvent, Watcher};
pub use write::{atomic_write, remove_file, rename_file};

use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Name of the app-data directory inside a vault. Always rebuildable from the Markdown files.
pub const APP_DIR: &str = ".vault";

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("watch error: {0}")]
    Watch(String),
}

pub type Result<T> = std::result::Result<T, FsError>;

pub(crate) fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> FsError + '_ {
    move |source| FsError::Io { path: path.to_path_buf(), source }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FileKind {
    Markdown,
    Attachment,
}

impl FileKind {
    pub fn of(rel: &str) -> FileKind {
        let lower = rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if rel.contains('.') && (lower == "md" || lower == "markdown") { FileKind::Markdown } else { FileKind::Attachment }
    }
}

/// One file found by a walk. `mtime_ns` is nanoseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    pub rel: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub kind: FileKind,
}

/// XXH3-64 of file contents: the change-detection hash.
#[inline]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(bytes)
}

/// Convert an absolute path under `root` into a normalized vault path.
pub fn normalize(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut out = String::new();
    for c in rel.components() {
        let s = c.as_os_str().to_str()?;
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(s);
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn to_abs(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

/// Whether a vault path is app-internal or hidden and must never be indexed.
pub fn is_ignored_rel(rel: &str, cfg: &ObsidianConfig) -> bool {
    rel.split('/').any(|seg| seg.starts_with('.') || seg == "node_modules") || cfg.is_user_ignored(rel)
}

pub fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

/// Parallel walk of the vault. Hidden directories (`.vault`, `.obsidian`, `.git`, ...) are skipped,
/// as are the user's Obsidian ignore filters. Results are sorted by path for determinism.
pub fn walk(root: &Path, cfg: &ObsidianConfig) -> Vec<WalkedFile> {
    let out = Mutex::new(Vec::with_capacity(4096));
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .follow_links(false)
        .threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .build_parallel();
    struct Local<'a> {
        buf: Vec<WalkedFile>,
        out: &'a Mutex<Vec<WalkedFile>>,
    }
    impl Drop for Local<'_> {
        fn drop(&mut self) {
            self.out.lock().append(&mut self.buf);
        }
    }
    walker.run(|| {
        let mut local = Local { buf: Vec::with_capacity(512), out: &out };
        Box::new(move |entry| {
            let Ok(entry) = entry else { return ignore::WalkState::Continue };
            let Some(ft) = entry.file_type() else { return ignore::WalkState::Continue };
            let Some(rel) = normalize(root, entry.path()) else { return ignore::WalkState::Continue };
            if cfg.is_user_ignored(&rel) || rel.split('/').any(|s| s == "node_modules") {
                return if ft.is_dir() { ignore::WalkState::Skip } else { ignore::WalkState::Continue };
            }
            if !ft.is_file() {
                return ignore::WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else { return ignore::WalkState::Continue };
            let kind = FileKind::of(&rel);
            local.buf.push(WalkedFile { size: meta.len(), mtime_ns: mtime_ns(&meta), kind, rel });
            ignore::WalkState::Continue
        })
    });
    let mut v = out.into_inner();
    v.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    v
}

/// List every directory in the vault (normalized), for folders that contain no notes yet.
pub fn walk_dirs(root: &Path, cfg: &ObsidianConfig) -> Vec<String> {
    let mut v: Vec<String> = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .filter_map(|e| normalize(root, e.path()))
        .filter(|r| !is_ignored_rel(r, cfg))
        .collect();
    v.sort();
    v
}

/// Stat a single vault file. `None` if it no longer exists or is not a regular file.
pub fn stat(root: &Path, rel: &str) -> Option<WalkedFile> {
    let meta = std::fs::metadata(to_abs(root, rel)).ok()?;
    meta.is_file().then(|| WalkedFile { rel: rel.to_string(), size: meta.len(), mtime_ns: mtime_ns(&meta), kind: FileKind::of(rel) })
}

pub fn read(root: &Path, rel: &str) -> Result<Vec<u8>> {
    let p = to_abs(root, rel);
    std::fs::read(&p).map_err(io_err(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_skips_hidden_and_classifies() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("a/b")).unwrap();
        std::fs::create_dir_all(r.join(".vault")).unwrap();
        std::fs::create_dir_all(r.join(".obsidian")).unwrap();
        std::fs::write(r.join("a/b/Note.md"), "# hi").unwrap();
        std::fs::write(r.join("a/pic.png"), [0u8; 4]).unwrap();
        std::fs::write(r.join(".vault/x.md"), "no").unwrap();
        std::fs::write(r.join(".obsidian/app.json"), "{}").unwrap();
        let files = walk(r, &ObsidianConfig::default());
        let rels: Vec<_> = files.iter().map(|f| (f.rel.as_str(), f.kind)).collect();
        assert_eq!(rels, vec![("a/b/Note.md", FileKind::Markdown), ("a/pic.png", FileKind::Attachment)]);
    }

    #[test]
    fn normalize_roundtrip() {
        let root = Path::new("/v");
        assert_eq!(normalize(root, Path::new("/v/a/b.md")).as_deref(), Some("a/b.md"));
        assert_eq!(to_abs(root, "a/b.md"), PathBuf::from("/v/a/b.md"));
        assert_eq!(normalize(root, root), None);
    }
}
