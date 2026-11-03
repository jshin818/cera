//! Obsidian link resolution: case-insensitive, shortest unique path match, aliases,
//! extension-less note names, and attachments by file name.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    File(u32),
    Unresolved,
}

/// Built from the file table; `ids[i]` is the file id of `paths[i]`.
#[derive(Debug, Default, Clone)]
pub struct LinkResolver {
    /// lowercase file name (with ".md" stripped for notes) -> candidates
    by_name: HashMap<String, Vec<(u32, u32)>>, // (file_id, path_len)
    /// lowercase full path (with and without .md) -> file id
    by_path: HashMap<String, u32>,
    /// lowercase alias -> candidates
    by_alias: HashMap<String, Vec<(u32, u32)>>,
}

fn strip_md(p: &str) -> &str {
    let lower_ends = |ext: &str| p.len() > ext.len() && p[p.len() - ext.len()..].eq_ignore_ascii_case(ext);
    if lower_ends(".md") {
        &p[..p.len() - 3]
    } else if lower_ends(".markdown") {
        &p[..p.len() - 9]
    } else {
        p
    }
}

impl LinkResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, id: u32, path: &str) {
        let lower = path.to_lowercase();
        let no_ext = strip_md(&lower).to_string();
        let name = no_ext.rsplit('/').next().unwrap_or(&no_ext).to_string();
        let len = path.len() as u32;
        self.by_path.insert(lower.clone(), id);
        self.by_path.insert(no_ext.clone(), id);
        let v = self.by_name.entry(name).or_default();
        v.push((id, len));
        v.sort_by_key(|&(i, l)| (l, i));
        if no_ext != lower {
            // Also allow [[note.md]].
            let full_name = lower.rsplit('/').next().unwrap_or(&lower).to_string();
            let v = self.by_name.entry(full_name).or_default();
            v.push((id, len));
            v.sort_by_key(|&(i, l)| (l, i));
        }
    }

    pub fn remove_file(&mut self, id: u32, path: &str) {
        let lower = path.to_lowercase();
        let no_ext = strip_md(&lower).to_string();
        self.by_path.remove(&lower);
        self.by_path.remove(&no_ext);
        for key in [no_ext.rsplit('/').next().unwrap_or(&no_ext).to_string(), lower.rsplit('/').next().unwrap_or(&lower).to_string()] {
            if let Some(v) = self.by_name.get_mut(&key) {
                v.retain(|&(i, _)| i != id);
                if v.is_empty() {
                    self.by_name.remove(&key);
                }
            }
        }
        self.by_alias.values_mut().for_each(|v| v.retain(|&(i, _)| i != id));
    }

    pub fn add_alias(&mut self, id: u32, path_len: u32, alias: &str) {
        let v = self.by_alias.entry(alias.to_lowercase()).or_default();
        if !v.iter().any(|&(i, _)| i == id) {
            v.push((id, path_len));
            v.sort_by_key(|&(i, l)| (l, i));
        }
    }

    /// Resolve a link target written in `from_path`. Empty targets (same-note links) resolve to `self_id`.
    pub fn resolve(&self, target: &str, from_path: &str, self_id: u32) -> Resolved {
        let t = target.trim().trim_start_matches("./").trim_start_matches('/');
        if t.is_empty() {
            return Resolved::File(self_id);
        }
        let lower = t.to_lowercase();
        // Relative markdown links: resolve against the source folder first.
        if let Some((dir, _)) = from_path.rsplit_once('/') {
            let rel = normalize_dots(&format!("{}/{}", dir.to_lowercase(), lower));
            if let Some(&id) = self.by_path.get(&rel).or_else(|| self.by_path.get(strip_md(&rel))) {
                return Resolved::File(id);
            }
        }
        if let Some(&id) = self.by_path.get(&lower).or_else(|| self.by_path.get(strip_md(&lower))) {
            return Resolved::File(id);
        }
        if lower.contains('/') {
            // Partial path: shortest path ending with "/<target>".
            let suffix = format!("/{}", strip_md(&lower));
            let name = suffix.rsplit('/').next().unwrap_or("");
            if let Some(cands) = self.by_name.get(name) {
                // Candidates are few; check suffix against stored path keys.
                for (path, &id) in self.by_path.iter().filter(|(p, _)| p.ends_with(&suffix)) {
                    let _ = path;
                    if cands.iter().any(|&(c, _)| c == id) {
                        return Resolved::File(self.shortest_among(cands, id));
                    }
                }
            }
            return Resolved::Unresolved;
        }
        if let Some(c) = self.by_name.get(&lower).or_else(|| self.by_name.get(strip_md(&lower))) {
            if let Some(&(id, _)) = c.first() {
                return Resolved::File(id);
            }
        }
        if let Some(c) = self.by_alias.get(&lower) {
            if let Some(&(id, _)) = c.first() {
                return Resolved::File(id);
            }
        }
        Resolved::Unresolved
    }

    fn shortest_among(&self, cands: &[(u32, u32)], fallback: u32) -> u32 {
        cands.iter().find(|&&(c, _)| c == fallback).map(|&(c, _)| c).unwrap_or(fallback)
    }
}

fn normalize_dots(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> LinkResolver {
        let mut r = LinkResolver::new();
        r.add_file(1, "Projects/Alpha.md");
        r.add_file(2, "Archive/Old/Alpha.md");
        r.add_file(3, "Beta.md");
        r.add_file(4, "assets/pic.png");
        r.add_alias(3, 7, "B-Note");
        r
    }

    #[test]
    fn resolves_like_obsidian() {
        let r = r();
        assert_eq!(r.resolve("alpha", "x.md", 0), Resolved::File(1), "shortest path wins");
        assert_eq!(r.resolve("Old/Alpha", "x.md", 0), Resolved::File(2));
        assert_eq!(r.resolve("Archive/Old/Alpha.md", "x.md", 0), Resolved::File(2));
        assert_eq!(r.resolve("BETA", "x.md", 0), Resolved::File(3));
        assert_eq!(r.resolve("b-note", "x.md", 0), Resolved::File(3));
        assert_eq!(r.resolve("pic.png", "x.md", 0), Resolved::File(4));
        assert_eq!(r.resolve("Missing", "x.md", 0), Resolved::Unresolved);
        assert_eq!(r.resolve("", "x.md", 9), Resolved::File(9));
        assert_eq!(r.resolve("../Beta.md", "Projects/Alpha.md", 1), Resolved::File(3));
    }

    #[test]
    fn remove_file_unresolves() {
        let mut r = r();
        r.remove_file(1, "Projects/Alpha.md");
        assert_eq!(r.resolve("alpha", "x.md", 0), Resolved::File(2));
    }
}
