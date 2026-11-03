//! Unlinked mentions: one Aho-Corasick pass (leftmost-longest, ASCII case-insensitive) over a note
//! finds every title/alias it mentions; a word-boundary check rejects partial-word hits.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Mention {
    pub target: u32,
    pub start: u32,
    pub end: u32,
}

pub struct MentionIndex {
    ac: Option<AhoCorasick>,
    targets: Vec<u32>,
}

impl MentionIndex {
    /// `names`: (file id, title or alias). Names shorter than `min_len` chars are skipped (too noisy).
    pub fn new(names: impl IntoIterator<Item = (u32, String)>, min_len: usize) -> MentionIndex {
        let mut pats = Vec::new();
        let mut targets = Vec::new();
        for (id, name) in names {
            let name = name.trim().to_string();
            if name.chars().count() >= min_len {
                pats.push(name);
                targets.push(id);
            }
        }
        let ac = (!pats.is_empty())
            .then(|| AhoCorasickBuilder::new().ascii_case_insensitive(true).match_kind(MatchKind::LeftmostLongest).build(&pats).expect("aho-corasick build"));
        MentionIndex { ac, targets }
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// All mentions in `text` whose span is not inside `skip` ranges (existing links, code) and
    /// whose target is not `self_id`.
    pub fn find(&self, text: &str, self_id: u32, skip: &[(u32, u32)]) -> Vec<Mention> {
        let Some(ac) = &self.ac else { return Vec::new() };
        let b = text.as_bytes();
        let boundary = |i: usize| -> bool {
            if i == 0 || i >= b.len() {
                return true;
            }
            let prev = text[..i].chars().next_back().unwrap_or(' ');
            let next = text[i..].chars().next().unwrap_or(' ');
            !(prev.is_alphanumeric() && next.is_alphanumeric())
        };
        ac.find_iter(text)
            .filter(|m| boundary(m.start()) && boundary(m.end()))
            .map(|m| Mention { target: self.targets[m.pattern().as_usize()], start: m.start() as u32, end: m.end() as u32 })
            .filter(|m| m.target != self_id && !skip.iter().any(|&(s, e)| m.start < e && m.end > s))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_whole_word_mentions() {
        let idx = MentionIndex::new([(1, "Rust".to_string()), (2, "Rust ownership".to_string()), (3, "Go".to_string()), (4, "Me".to_string())], 3);
        let text = "I like rust ownership and Rusty things. [[Rust]] here. rust.";
        let m = idx.find(text, 99, &[(text.find("[[").unwrap() as u32, (text.find("]]").unwrap() + 2) as u32)]);
        let got: Vec<(u32, &str)> = m.iter().map(|x| (x.target, &text[x.start as usize..x.end as usize])).collect();
        assert_eq!(got, vec![(2, "rust ownership"), (1, "rust")]);
    }
}
