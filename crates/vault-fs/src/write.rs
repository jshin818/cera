//! Atomic writes: write a temp file in the same directory, fsync, then rename over the target.

use crate::{io_err, to_abs, Result};
use std::io::Write;
use std::path::Path;

/// Atomically write `bytes` to the vault path `rel`. Returns the XXH3 hash of the content, which
/// the engine records so the watcher can recognise (and ignore) the app's own writes.
pub fn atomic_write(root: &Path, rel: &str, bytes: &[u8]) -> Result<u64> {
    let target = to_abs(root, rel);
    let dir = target.parent().unwrap_or(root);
    std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.vault-tmp-{}-{seq}", target.file_name().and_then(|s| s.to_str()).unwrap_or("note"), std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(io_err(&tmp))?;
        f.write_all(bytes).map_err(io_err(&tmp))?;
        f.sync_all().map_err(io_err(&tmp))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err(&target)(e));
    }
    Ok(crate::hash_bytes(bytes))
}

pub fn remove_file(root: &Path, rel: &str) -> Result<()> {
    let p = to_abs(root, rel);
    std::fs::remove_file(&p).map_err(io_err(&p))
}

/// Rename within the vault, creating parent folders. Fails if the target exists.
pub fn rename_file(root: &Path, from: &str, to: &str) -> Result<()> {
    let a = to_abs(root, from);
    let b = to_abs(root, to);
    if b.exists() {
        return Err(io_err(&b)(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "target exists")));
    }
    if let Some(dir) = b.parent() {
        std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    std::fs::rename(&a, &b).map_err(io_err(&a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_dirs_and_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let h = atomic_write(d.path(), "x/y/z.md", b"hello").unwrap();
        assert_eq!(h, crate::hash_bytes(b"hello"));
        assert_eq!(std::fs::read(d.path().join("x/y/z.md")).unwrap(), b"hello");
        let names: Vec<_> = std::fs::read_dir(d.path().join("x/y")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1);
    }
}
