//! Read-only view of `.obsidian/app.json`. Vault never writes inside `.obsidian/`.

use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct ObsidianConfig {
    /// `userIgnoreFilters`: path prefixes (Obsidian also allows regexes `/.../`; we support prefixes and globs-lite).
    pub ignore_filters: Vec<String>,
    /// `attachmentFolderPath`: "/" = vault root, "./" = same folder as note, "./sub" = subfolder of note, else a vault folder.
    pub attachment_folder: Option<String>,
}

impl ObsidianConfig {
    pub fn load(root: &Path) -> ObsidianConfig {
        let Ok(text) = std::fs::read_to_string(root.join(".obsidian").join("app.json")) else {
            return ObsidianConfig::default();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            tracing::warn!("could not parse .obsidian/app.json; ignoring it");
            return ObsidianConfig::default();
        };
        let ignore_filters = v
            .get("userIgnoreFilters")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str()).map(|s| s.trim_end_matches('/').to_string()).collect())
            .unwrap_or_default();
        let attachment_folder = v.get("attachmentFolderPath").and_then(|s| s.as_str()).map(str::to_string);
        ObsidianConfig { ignore_filters, attachment_folder }
    }

    pub fn is_user_ignored(&self, rel: &str) -> bool {
        self.ignore_filters
            .iter()
            .any(|f| !f.is_empty() && (rel == f || rel.starts_with(&format!("{f}/")) || (!f.contains('/') && rel.split('/').any(|seg| seg == f))))
    }

    /// Folder (normalized, may be "") where a new attachment pasted into `note_rel` should go.
    pub fn attachment_dir_for(&self, note_rel: &str) -> String {
        let note_dir = note_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        match self.attachment_folder.as_deref() {
            None | Some("/") | Some("") => String::new(),
            Some("./") | Some(".") => note_dir.to_string(),
            Some(s) if s.starts_with("./") => {
                let sub = s.trim_start_matches("./").trim_end_matches('/');
                if note_dir.is_empty() { sub.to_string() } else { format!("{note_dir}/{sub}") }
            }
            Some(s) => s.trim_matches('/').to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_app_json() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".obsidian")).unwrap();
        std::fs::write(d.path().join(".obsidian/app.json"), r#"{"userIgnoreFilters":["Archive/","templates"],"attachmentFolderPath":"./assets"}"#).unwrap();
        let c = ObsidianConfig::load(d.path());
        assert!(c.is_user_ignored("Archive/old.md"));
        assert!(c.is_user_ignored("x/templates/t.md"));
        assert!(!c.is_user_ignored("Archived.md"));
        assert_eq!(c.attachment_dir_for("a/b.md"), "a/assets");
        assert_eq!(c.attachment_dir_for("b.md"), "assets");
    }
}
