//! Debounced watcher (FSEvents on macOS, inotify on Linux). Installed before the startup walk
//! finishes; consumers dedupe by path + hash. Rescan flags are surfaced as `WatchEvent::Rescan`.

use crate::{ObsidianConfig, normalize};
use crossbeam_channel::{Receiver, Sender};
use notify_debouncer_full::notify::{self, EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Files that may have been created or modified (normalized vault paths).
    Changed(Vec<String>),
    /// Files that were removed.
    Removed(Vec<String>),
    /// The OS dropped or coalesced events; do a full size+mtime verify.
    Rescan,
}

pub struct Watcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    pub events: Receiver<WatchEvent>,
}

impl Watcher {
    pub fn start(root: &Path, cfg: ObsidianConfig, debounce: Duration) -> crate::Result<Watcher> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let root_buf: PathBuf = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root2 = root_buf.clone();
        let mut debouncer = new_debouncer(debounce, None, move |res: DebounceEventResult| {
            handle(&root2, &cfg, &tx, res);
        })
        .map_err(|e| crate::FsError::Watch(e.to_string()))?;
        debouncer.watch(&root_buf, RecursiveMode::Recursive).map_err(|e| crate::FsError::Watch(e.to_string()))?;
        Ok(Watcher { _debouncer: debouncer, events: rx })
    }
}

fn handle(root: &Path, cfg: &ObsidianConfig, tx: &Sender<WatchEvent>, res: DebounceEventResult) {
    let events = match res {
        Ok(ev) => ev,
        Err(errs) => {
            tracing::warn!(?errs, "watcher errors; scheduling rescan");
            let _ = tx.send(WatchEvent::Rescan);
            return;
        }
    };
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for ev in events {
        if ev.need_rescan() {
            let _ = tx.send(WatchEvent::Rescan);
            continue;
        }
        let rels = ev.paths.iter().filter_map(|p| normalize(root, p)).filter(|r| !crate::is_ignored_rel(r, cfg));
        match ev.kind {
            EventKind::Remove(_) => removed.extend(rels),
            EventKind::Create(_) | EventKind::Modify(_) => {
                // Renames arrive as Modify(Name); the engine stats each path and treats missing as removed.
                changed.extend(rels)
            }
            EventKind::Any | EventKind::Other => {
                let _ = tx.send(WatchEvent::Rescan);
            }
            EventKind::Access(_) => {}
        }
    }
    changed.sort();
    changed.dedup();
    removed.sort();
    removed.dedup();
    if !changed.is_empty() {
        let _ = tx.send(WatchEvent::Changed(changed));
    }
    if !removed.is_empty() {
        let _ = tx.send(WatchEvent::Removed(removed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_external_write() {
        let d = tempfile::tempdir().unwrap();
        let w = Watcher::start(d.path(), ObsidianConfig::default(), Duration::from_millis(50)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(d.path().join("n.md"), "x").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(WatchEvent::Changed(v)) = w.events.recv_timeout(Duration::from_millis(200))
                && v.iter().any(|p| p == "n.md")
            {
                return;
            }
        }
        panic!("no event for n.md");
    }
}
