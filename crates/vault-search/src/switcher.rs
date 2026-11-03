//! Quick switcher: nucleo fuzzy scoring over titles, aliases and paths, boosted by frecency and
//! PageRank; frizbee (typo-tolerant Smith-Waterman) fills in when nucleo finds fewer than 5.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use rayon::prelude::*;

#[derive(Debug, Clone)]
pub struct SwitchItem {
    pub id: u32,
    pub title: String,
    pub aliases: Vec<String>,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SwitchHit {
    pub id: u32,
    pub score: f32,
    /// Which alias matched, if the hit came from an alias.
    pub alias: Option<String>,
    pub typo: bool,
}

struct Entry {
    id: u32,
    title: Utf32String,
    title_lower: String,
    aliases: Vec<(Utf32String, String)>,
    path: Utf32String,
}

/// Immutable switcher index. Rebuild (cheap, ~O(n)) or use `with_changes` on snapshot change.
pub struct Switcher {
    entries: Vec<Entry>,
    /// boost per entry index: 1 + 0.5*frecency_norm + 0.2*pagerank_norm
    boost: Vec<f32>,
    titles_lower: Vec<String>,
}

const CHUNK: usize = 4096;

impl Switcher {
    pub fn new(items: Vec<SwitchItem>, frecency_norm: impl Fn(u32) -> f32, pagerank_norm: impl Fn(u32) -> f32) -> Switcher {
        let entries: Vec<Entry> = items
            .into_par_iter()
            .map(|it| Entry {
                id: it.id,
                title_lower: it.title.to_lowercase(),
                title: Utf32String::from(it.title.as_str()),
                aliases: it.aliases.into_iter().map(|a| (Utf32String::from(a.as_str()), a)).collect(),
                path: Utf32String::from(it.path.trim_end_matches(".md")),
            })
            .collect();
        let boost = entries.iter().map(|e| 1.0 + 0.5 * frecency_norm(e.id) + 0.2 * pagerank_norm(e.id)).collect();
        let titles_lower = entries.iter().map(|e| e.title_lower.clone()).collect();
        Switcher { entries, boost, titles_lower }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn query(&self, q: &str, limit: usize) -> Vec<SwitchHit> {
        let q = q.trim();
        if q.is_empty() {
            // Empty query: most frecent first.
            let mut v: Vec<SwitchHit> = self
                .entries
                .iter()
                .zip(&self.boost)
                .filter(|(_, b)| **b > 1.0)
                .map(|(e, b)| SwitchHit { id: e.id, score: *b, alias: None, typo: false })
                .collect();
            v.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
            v.truncate(limit);
            return v;
        }
        let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        let ql = q.to_lowercase();
        let mut hits: Vec<SwitchHit> = self
            .entries
            .par_chunks(CHUNK)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let mut m = Matcher::new(Config::DEFAULT);
                let mut local = Vec::new();
                for (k, e) in chunk.iter().enumerate() {
                    let idx = ci * CHUNK + k;
                    let mut best: Option<(u32, Option<&str>)> = pattern.score(e.title.slice(..), &mut m).map(|s| (s, None));
                    for (a32, a) in &e.aliases {
                        if let Some(s) = pattern.score(a32.slice(..), &mut m) {
                            let s = s * 9 / 10;
                            if best.is_none_or(|b| s > b.0) {
                                best = Some((s, Some(a.as_str())));
                            }
                        }
                    }
                    if best.is_none() {
                        if let Some(s) = pattern.score(e.path.slice(..), &mut m) {
                            best = Some((s / 2, None));
                        }
                    }
                    // An exact alias match beats a fuzzy title match on the same note.
                    if e.title_lower != ql {
                        if let Some((_, a)) = e.aliases.iter().find(|(_, a)| a.to_lowercase() == ql) {
                            best = Some((best.map(|b| b.0).unwrap_or(0), Some(a.as_str())));
                        }
                    }
                    if let Some((s, alias)) = best {
                        let mut score = s as f32 * self.boost[idx];
                        if e.title_lower == ql || alias.is_some_and(|a| a.to_lowercase() == ql) {
                            score += 1.0e9; // exact title (or alias) match always ranks first
                        }
                        local.push(SwitchHit { id: e.id, score, alias: alias.map(str::to_string), typo: false });
                    }
                }
                local
            })
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        hits.truncate(limit);
        if hits.len() < 5 && q.chars().count() >= 3 {
            self.typo_fallback(q, &mut hits, limit);
        }
        hits
    }

    fn typo_fallback(&self, q: &str, hits: &mut Vec<SwitchHit>, limit: usize) {
        let max_typos = match q.chars().count() {
            0..=3 => 1,
            4..=7 => 1,
            _ => 2,
        };
        let cfg = frizbee::Config { max_typos: Some(max_typos), ..Default::default() };
        let mut m = frizbee::Matcher::new(q, &cfg);
        let mut extra = m.match_list_parallel(&self.titles_lower, 0);
        extra.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
        for x in extra {
            if hits.len() >= limit.max(5) {
                break;
            }
            let e = &self.entries[x.index as usize];
            if hits.iter().any(|h| h.id == e.id) {
                continue;
            }
            hits.push(SwitchHit { id: e.id, score: x.score as f32 * 0.01, alias: None, typo: true });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<SwitchItem> {
        ["Rust ownership", "Rusty nail", "Meeting notes", "Recipes/Ramen", "rust"]
            .iter()
            .enumerate()
            .map(|(i, t)| SwitchItem { id: i as u32, title: t.rsplit('/').next().unwrap().to_string(), aliases: if i == 2 { vec!["standup".into()] } else { vec![] }, path: format!("{t}.md") })
            .collect()
    }

    #[test]
    fn exact_first_alias_and_typo() {
        let s = Switcher::new(items(), |_| 0.0, |_| 0.0);
        let r = s.query("rust", 10);
        assert_eq!(r[0].id, 4, "exact title first");
        assert!(r.iter().any(|h| h.id == 0));
        let r = s.query("standup", 10);
        assert_eq!(r[0].id, 2);
        assert_eq!(r[0].alias.as_deref(), Some("standup"));
        let r = s.query("meetnig", 10);
        assert!(r.iter().any(|h| h.id == 2 && h.typo), "{r:?}");
        let r = s.query("recipes", 10);
        assert_eq!(r[0].id, 3, "path match");
    }

    #[test]
    fn frecency_boost_reorders() {
        let s = Switcher::new(items(), |id| if id == 1 { 1.0 } else { 0.0 }, |_| 0.0);
        let r = s.query("rus", 10);
        let pos1 = r.iter().position(|h| h.id == 1).unwrap();
        let pos0 = r.iter().position(|h| h.id == 0).unwrap();
        assert!(pos1 < pos0);
        assert_eq!(s.query("", 10)[0].id, 1);
    }
}
