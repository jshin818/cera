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
    /// Character-class bitmask of title + aliases + path (a-z, 0-9, other); all bits set if the
    /// text has non-ASCII letters (accent folding could match them). A query whose characters are
    /// not all present cannot match, so the fuzzy scorer is skipped.
    mask: u64,
    title: Utf32String,
    title_lower: String,
    aliases: Vec<(Utf32String, String)>,
    path: Utf32String,
}

/// Switcher index. Rebuilt when titles change; frecency boosts update in place.
pub struct Switcher {
    entries: Vec<Entry>,
    /// boost per entry index (f32 bits): 1 + 0.5*frecency_norm + 0.2*pagerank_norm
    boost: Vec<std::sync::atomic::AtomicU32>,
    pagerank_part: Vec<f32>,
    by_id: std::collections::HashMap<u32, usize>,
    titles_lower: Vec<String>,
    /// Incremental narrowing: the previous query and the entry indices it matched. Typing one
    /// more character can only shrink the match set, so only those entries are rescored.
    last: parking_lot::Mutex<Option<(String, std::sync::Arc<Vec<u32>>)>>,
}

const CHUNK: usize = 4096;

impl Switcher {
    pub fn new(items: Vec<SwitchItem>, frecency_norm: impl Fn(u32) -> f32, pagerank_norm: impl Fn(u32) -> f32) -> Switcher {
        let entries: Vec<Entry> = items
            .into_par_iter()
            .map(|it| Entry {
                mask: char_mask(&it.title) | it.aliases.iter().map(|a| char_mask(a)).fold(0, |a, b| a | b) | char_mask(&it.path),
                id: it.id,
                title_lower: it.title.to_lowercase(),
                title: Utf32String::from(it.title.as_str()),
                aliases: it.aliases.into_iter().map(|a| (Utf32String::from(a.as_str()), a)).collect(),
                path: Utf32String::from(it.path.trim_end_matches(".md")),
            })
            .collect();
        let pagerank_part: Vec<f32> = entries.iter().map(|e| 0.2 * pagerank_norm(e.id)).collect();
        let boost = entries.iter().zip(&pagerank_part).map(|(e, p)| std::sync::atomic::AtomicU32::new((1.0 + 0.5 * frecency_norm(e.id) + p).to_bits())).collect();
        let titles_lower = entries.iter().map(|e| e.title_lower.clone()).collect();
        let by_id = entries.iter().enumerate().map(|(i, e)| (e.id, i)).collect();
        Switcher { entries, boost, pagerank_part, by_id, titles_lower, last: parking_lot::Mutex::new(None) }
    }

    fn boost(&self, idx: usize) -> f32 {
        f32::from_bits(self.boost[idx].load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Update one note's frecency (normalized 0..1) without rebuilding.
    pub fn set_frecency(&self, id: u32, frecency_norm: f32) {
        if let Some(&i) = self.by_id.get(&id) {
            self.boost[i].store((1.0 + 0.5 * frecency_norm.clamp(0.0, 1.0) + self.pagerank_part[i]).to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
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
                .enumerate()
                .map(|(i, e)| (e, self.boost(i)))
                .filter(|(_, b)| *b > 1.0)
                .map(|(e, b)| SwitchHit { id: e.id, score: b, alias: None, typo: false })
                .collect();
            v.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
            v.truncate(limit);
            return v;
        }
        let pattern = Pattern::parse(q, CaseMatching::Ignore, Normalization::Smart);
        // Prefilter bits: only plain fuzzy queries (negation/anchors change what must be present).
        let qmask = if q.contains(['!', '^', '$', '\'', '|']) { 0 } else { char_mask(q) & !OTHER };
        let ql = q.to_lowercase();
        let prev = {
            let l = self.last.lock();
            l.as_ref().filter(|(pq, _)| ql.starts_with(pq.as_str()) && !pq.is_empty() && !pq.contains(['!', '^', '\'', '$', '|'])).map(|(_, v)| v.clone())
        };
        let all: Vec<u32>;
        let cand: &[u32] = match &prev {
            Some(v) => v,
            None => {
                all = (0..self.entries.len() as u32).collect();
                &all
            }
        };
        let scored: Vec<(u32, SwitchHit)> = cand
            .par_chunks(CHUNK)
            .flat_map_iter(|chunk| {
                let mut m = Matcher::new(Config::DEFAULT);
                let mut local = Vec::new();
                for &idx in chunk {
                    let idx = idx as usize;
                    let e = &self.entries[idx];
                    if e.mask & qmask != qmask {
                        continue;
                    }
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
                        let mut score = s as f32 * self.boost(idx);
                        if e.title_lower == ql || alias.is_some_and(|a| a.to_lowercase() == ql) {
                            score += 1.0e9; // exact title (or alias) match always ranks first
                        }
                        local.push((idx as u32, SwitchHit { id: e.id, score, alias: alias.map(str::to_string), typo: false }));
                    }
                }
                local
            })
            .collect();
        *self.last.lock() = Some((ql.clone(), std::sync::Arc::new(scored.iter().map(|x| x.0).collect())));
        let mut hits: Vec<SwitchHit> = scored.into_iter().map(|x| x.1).collect();
        let k = limit.min(hits.len());
        if k > 0 && k < hits.len() {
            hits.select_nth_unstable_by(k - 1, |a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
            hits.truncate(k);
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
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

const OTHER: u64 = 1 << 36;

fn char_mask(s: &str) -> u64 {
    let mut m = 0u64;
    for c in s.chars() {
        match c {
            'a'..='z' => m |= 1 << (c as u32 - 'a' as u32),
            'A'..='Z' => m |= 1 << (c as u32 - 'A' as u32),
            '0'..='9' => m |= 1 << (26 + c as u32 - '0' as u32),
            c if c.is_whitespace() => {}
            c if c.is_ascii() => m |= OTHER,
            _ => return u64::MAX,
        }
    }
    m
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
