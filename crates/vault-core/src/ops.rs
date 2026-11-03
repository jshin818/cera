//! Write path: every edit writes Markdown atomically (temp + rename), records the hash so the
//! watcher ignores the echo, and flows through the same apply step as external edits.

use crate::indexer::{apply, parse_update};
use crate::{CoreError, Engine, Result};
use serde::Serialize;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct NoteView {
    pub id: Option<u32>,
    pub path: String,
    pub title: String,
    pub content: String,
    /// XXH3 of the content as hex, passed back on save for conflict detection.
    pub hash: String,
    pub mtime_ms: i64,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub words: u32,
    pub backlinks: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SaveOutcome {
    pub path: String,
    pub hash: String,
    pub generation: u64,
}

pub(crate) fn hex(h: u64) -> String {
    format!("{h:016x}")
}

fn check_rel(path: &str) -> Result<String> {
    let p = path.trim().trim_matches('/').replace('\\', "/");
    if p.is_empty() || p.split('/').any(|s| s.is_empty() || s == ".." || s == "." || s.starts_with('.')) {
        return Err(CoreError::Invalid(format!("bad path: {path}")));
    }
    Ok(p)
}

/// File-name-safe version of a title (Obsidian forbids these characters in names).
pub fn safe_name(title: &str) -> String {
    let s: String = title.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']') { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_start_matches('.').trim();
    if s.is_empty() { "Untitled".to_string() } else { s.chars().take(120).collect() }
}

impl Engine {
    fn record_write(&self, path: &str, bytes: &[u8]) -> Result<u64> {
        let h = vault_fs::atomic_write(&self.0.root, path, bytes)?;
        self.0.own_writes.lock().insert(path.to_string(), h);
        Ok(h)
    }

    fn reindex_now(&self, path: &str, content: &str, hash: u64) -> u64 {
        let st = vault_fs::stat(&self.0.root, path);
        let (size, mtime) = st.map(|s| (s.size, s.mtime_ns)).unwrap_or((content.len() as u64, 0));
        let upd = if vault_fs::FileKind::of(path) == vault_fs::FileKind::Markdown {
            parse_update(&self.0, path, size, mtime, hash, content)
        } else {
            crate::state::FileUpdate { rel: path.to_string(), size, mtime_ns: mtime, hash, parsed: None, chunks: Vec::new(), minhash: None }
        };
        apply(self, vec![upd], Vec::new(), false)
    }

    pub fn read_note(&self, path: &str) -> Result<NoteView> {
        let path = check_rel(path)?;
        let bytes = vault_fs::read(&self.0.root, &path).map_err(|_| CoreError::NotFound(path.clone()))?;
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let snap = self.snapshot();
        let e = snap.by_path(&path);
        Ok(NoteView {
            id: e.map(|e| e.id),
            title: e.map(|e| e.title.clone()).unwrap_or_else(|| vault_parse::title_from_path(&path)),
            hash: hex(vault_fs::hash_bytes(&bytes)),
            mtime_ms: e.map(|e| e.mtime_ns / 1_000_000).unwrap_or(0),
            tags: e.map(|e| e.tags.clone()).unwrap_or_default(),
            aliases: e.map(|e| e.aliases.clone()).unwrap_or_default(),
            words: e.map(|e| e.word_count).unwrap_or(0),
            backlinks: e.map(|e| e.backlinks.len()).unwrap_or(0),
            path,
            content,
        })
    }

    /// Save a note. With `base_hash`, refuses to overwrite a file that changed on disk since it was
    /// loaded (returns `Conflict`), unless the disk already holds exactly this content.
    pub fn save_note(&self, path: &str, content: &str, base_hash: Option<&str>) -> Result<SaveOutcome> {
        let path = check_rel(path)?;
        if vault_fs::FileKind::of(&path) != vault_fs::FileKind::Markdown {
            return Err(CoreError::Invalid("only Markdown notes can be saved".into()));
        }
        let new_hash = vault_fs::hash_bytes(content.as_bytes());
        if let Some(base) = base_hash {
            if let Ok(disk) = vault_fs::read(&self.0.root, &path) {
                let dh = vault_fs::hash_bytes(&disk);
                if hex(dh) != base && dh != new_hash {
                    return Err(CoreError::Conflict { disk_hash: dh });
                }
                if dh == new_hash {
                    return Ok(SaveOutcome { path, hash: hex(dh), generation: self.snapshot().generation });
                }
            }
        }
        let h = self.record_write(&path, content.as_bytes())?;
        let generation = self.reindex_now(&path, content, h);
        self.touch(&path, 2.0);
        Ok(SaveOutcome { path, hash: hex(h), generation })
    }

    /// Create a note in `folder` with a unique name derived from `title`.
    pub fn create_note(&self, folder: &str, title: &str, content: &str) -> Result<SaveOutcome> {
        let folder = folder.trim().trim_matches('/');
        if !folder.is_empty() {
            check_rel(folder)?;
        }
        let base = safe_name(title);
        let snap = self.snapshot();
        let mut name = base.clone();
        let mut i = 1;
        let full = |n: &str| if folder.is_empty() { format!("{n}.md") } else { format!("{folder}/{n}.md") };
        while snap.by_path.contains_key(&full(&name)) || vault_fs::to_abs(&self.0.root, &full(&name)).exists() {
            name = format!("{base} {i}");
            i += 1;
        }
        let path = full(&name);
        let h = self.record_write(&path, content.as_bytes())?;
        let generation = self.reindex_now(&path, content, h);
        self.touch(&path, 2.0);
        if !self.near_duplicates(&path).is_empty() {
            tracing::info!(%path, "new note looks like a near-duplicate");
        }
        Ok(SaveOutcome { path, hash: hex(h), generation })
    }

    /// Rename or move a note/attachment, rewriting links to it in every note that links it.
    pub fn rename(&self, from: &str, to: &str) -> Result<String> {
        let from = check_rel(from)?;
        let mut to = check_rel(to)?;
        if vault_fs::FileKind::of(&from) == vault_fs::FileKind::Markdown && vault_fs::FileKind::of(&to) != vault_fs::FileKind::Markdown {
            to.push_str(".md");
        }
        if from == to {
            return Ok(to);
        }
        let snap = self.snapshot();
        let e = snap.by_path(&from).ok_or_else(|| CoreError::NotFound(from.clone()))?.clone();
        if vault_fs::to_abs(&self.0.root, &to).exists() && !from.eq_ignore_ascii_case(&to) {
            return Err(CoreError::Exists(to));
        }
        vault_fs::rename_file(&self.0.root, &from, &to)?;
        // Index the move first so resolution sees the new name.
        let st = vault_fs::stat(&self.0.root, &to).ok_or_else(|| CoreError::NotFound(to.clone()))?;
        let bytes = vault_fs::read(&self.0.root, &to)?;
        let h = vault_fs::hash_bytes(&bytes);
        let upd = if e.markdown {
            parse_update(&self.0, &to, st.size, st.mtime_ns, h, &String::from_utf8_lossy(&bytes))
        } else {
            crate::state::FileUpdate { rel: to.clone(), size: st.size, mtime_ns: st.mtime_ns, hash: h, parsed: None, chunks: Vec::new(), minhash: None }
        };
        self.0.own_writes.lock().insert(to.clone(), h);
        apply(self, vec![upd], vec![from.clone()], false);
        // Rewrite links in sources (they were resolved to the old id).
        let new_snap = self.snapshot();
        let new_id = new_snap.by_path.get(&to).copied();
        let unique_name = {
            let stem = vault_parse::title_from_path(&to).to_lowercase();
            new_snap.iter().filter(|x| vault_parse::title_from_path(&x.path).to_lowercase() == stem && x.markdown == e.markdown).count() <= 1
        };
        let new_target = |embed_or_md_name: bool| -> String {
            let no_ext = to.strip_suffix(".md").unwrap_or(&to);
            if unique_name {
                if e.markdown { vault_parse::title_from_path(&to) } else { to.rsplit('/').next().unwrap_or(&to).to_string() }
            } else if embed_or_md_name || e.markdown {
                no_ext.to_string()
            } else {
                to.clone()
            }
        };
        for src_id in &e.backlinks {
            let Some(src) = snap.get(*src_id) else { continue };
            let src_path = if src.path == from { to.clone() } else { src.path.clone() };
            let Ok(bytes) = vault_fs::read(&self.0.root, &src_path) else { continue };
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            let mut links: Vec<&crate::OutLink> = src.links.iter().filter(|l| l.dst() == Some(e.id)).collect();
            links.sort_by(|a, b| b.start.cmp(&a.start));
            let mut changed = false;
            for l in links {
                let (s, en) = (l.start as usize, l.end() as usize);
                if en > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(en) {
                    continue;
                }
                let old = &text[s..en];
                let replacement = if old.starts_with("[[") || old.starts_with("![[") {
                    let bang = if old.starts_with('!') { "!" } else { "" };
                    let inner = old.trim_start_matches('!').trim_start_matches("[[").trim_end_matches("]]");
                    let (target_part, alias) = match inner.split_once('|') {
                        Some((a, b)) => (a, Some(b)),
                        None => (inner, None),
                    };
                    let sub = target_part.find(['#', '^']).map(|i| &target_part[i..]).unwrap_or("");
                    let mut r = format!("{bang}[[{}{sub}", new_target(false));
                    if let Some(a) = alias {
                        r.push('|');
                        r.push_str(a);
                    }
                    r.push_str("]]");
                    r
                } else if let (Some(close), true) = (old.rfind("]("), old.ends_with(')')) {
                    let url = &old[close + 2..old.len() - 1];
                    let sub = url.find('#').map(|i| &url[i..]).unwrap_or("");
                    format!("{}]({}{sub})", &old[..close], to.replace(' ', "%20"))
                } else {
                    continue;
                };
                text.replace_range(s..en, &replacement);
                changed = true;
            }
            if changed {
                let h = self.record_write(&src_path, text.as_bytes())?;
                self.reindex_now(&src_path, &text, h);
            }
        }
        let _ = new_id;
        Ok(to)
    }

    /// Move a file to the vault's `.trash/` folder (Obsidian's local-trash convention).
    pub fn delete(&self, path: &str) -> Result<()> {
        let path = check_rel(path)?;
        let src = vault_fs::to_abs(&self.0.root, &path);
        if !src.exists() {
            return Err(CoreError::NotFound(path));
        }
        let trash = self.0.root.join(".trash");
        std::fs::create_dir_all(&trash)?;
        let name = path.rsplit('/').next().unwrap_or(&path);
        let mut dst = trash.join(name);
        let mut i = 1;
        while dst.exists() {
            dst = trash.join(format!("{} {i}", name));
            i += 1;
        }
        std::fs::rename(&src, &dst)?;
        apply(self, Vec::new(), vec![path], false);
        Ok(())
    }

    pub fn create_folder(&self, path: &str) -> Result<String> {
        let path = check_rel(path)?;
        std::fs::create_dir_all(vault_fs::to_abs(&self.0.root, &path))?;
        crate::indexer::process_paths(self, vec![path.clone()], false);
        Ok(path)
    }

    /// Rename/move a folder. Links by note name keep resolving; path-qualified links re-resolve.
    pub fn rename_folder(&self, from: &str, to: &str) -> Result<String> {
        let from = check_rel(from)?;
        let to = check_rel(to)?;
        let a = vault_fs::to_abs(&self.0.root, &from);
        let b = vault_fs::to_abs(&self.0.root, &to);
        if b.exists() {
            return Err(CoreError::Exists(to));
        }
        if let Some(p) = b.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(&a, &b)?;
        crate::indexer::process_paths(self, vec![from, to.clone()], false);
        Ok(to)
    }

    pub fn delete_folder(&self, path: &str) -> Result<()> {
        let path = check_rel(path)?;
        let src = vault_fs::to_abs(&self.0.root, &path);
        let trash = self.0.root.join(".trash");
        std::fs::create_dir_all(&trash)?;
        let name = path.rsplit('/').next().unwrap_or(&path);
        let mut dst = trash.join(name);
        let mut i = 1;
        while dst.exists() {
            dst = trash.join(format!("{name} {i}"));
            i += 1;
        }
        std::fs::rename(&src, &dst)?;
        crate::indexer::process_paths(self, vec![path], false);
        Ok(())
    }

    /// Save a pasted/dropped attachment next to `note_path` per Obsidian's attachment setting.
    pub fn save_attachment(&self, note_path: &str, file_name: &str, bytes: &[u8]) -> Result<String> {
        let dir = self.0.cfg.attachment_dir_for(note_path);
        let clean = safe_name(file_name.rsplit_once('.').map(|(a, _)| a).unwrap_or(file_name));
        let ext = file_name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or_else(|| "bin".into());
        let mut i = 0;
        let path = loop {
            let n = if i == 0 { format!("{clean}.{ext}") } else { format!("{clean} {i}.{ext}") };
            let p = if dir.is_empty() { n } else { format!("{dir}/{n}") };
            if !vault_fs::to_abs(&self.0.root, &p).exists() {
                break p;
            }
            i += 1;
        };
        let h = self.record_write(&path, bytes)?;
        self.reindex_now(&path, "", h);
        Ok(path)
    }

    /// Turn an unlinked mention into a link: `[[Title]]`, or `[[Title|as written]]` when the case differs.
    pub fn link_mention(&self, src_path: &str, start: u32, end: u32, target_path: &str) -> Result<SaveOutcome> {
        let snap = self.snapshot();
        let target = snap.by_path(target_path).ok_or_else(|| CoreError::NotFound(target_path.into()))?;
        let bytes = vault_fs::read(&self.0.root, src_path)?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        let (s, e) = (start as usize, end as usize);
        if e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
            return Err(CoreError::Invalid("stale mention offsets".into()));
        }
        let written = text[s..e].to_string();
        let name = if target.title.eq_ignore_ascii_case(&written) || target.aliases.iter().any(|a| a == &written) { None } else { Some(written.clone()) };
        let link = match name {
            None if written == target.title || target.aliases.contains(&written) => format!("[[{written}]]"),
            _ => format!("[[{}|{}]]", target.title, written),
        };
        text.replace_range(s..e, &link);
        self.save_note(src_path, &text, None)
    }

    /// Frecency bump: open = 1, edit = 2.
    pub fn touch(&self, path: &str, weight: f32) {
        let now = crate::now_ms();
        let mut st = self.0.state.lock();
        let Some(&id) = st.by_path.get(path) else { return };
        if let Some(f) = st.frecency.get_mut(id as usize) {
            f.bump(now, weight);
            let rec = vault_store::FrecencyRec { score: f.score, last_access_ms: f.last_access_ms };
            self.0.store_write(vault_store::StoreBatch { events: vec![(id, rec)], ..Default::default() });
        }
        crate::indexer::publish(&self.0, &st);
        drop(st);
        if let Some(sw) = self.0.switcher.load_full() {
            let snap = self.snapshot();
            sw.set_frecency(id, snap.frecency_norm(id, now));
        }
        self.0.dirty_persist.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Opaque UI workspace state (open tabs, last note, theme), stored in `.vault/workspace.json`.
    pub fn workspace(&self) -> serde_json::Value {
        std::fs::read(self.0.app_dir.join("workspace.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
    }

    pub fn set_workspace(&self, v: &serde_json::Value) -> Result<()> {
        let p = self.0.app_dir.join("workspace.json");
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }
}

/// The first-frame path: read the workspace's last note straight from disk, without waiting for
/// the index. Returns (path, content, elapsed ms).
pub fn first_frame(root: &std::path::Path) -> Option<(String, String, f64)> {
    let t = Instant::now();
    let ws: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join(vault_fs::APP_DIR).join("workspace.json")).ok()?).ok()?;
    let last = ws.get("last")?.as_str()?.to_string();
    let content = std::fs::read_to_string(vault_fs::to_abs(root, &last)).ok()?;
    Some((last, content, crate::ms(t)))
}
