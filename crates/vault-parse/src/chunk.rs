//! Chunking for embeddings: split by headings, then into ~200-400 word windows with a one-sentence overlap.

use crate::ParsedNote;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Byte range in the source file.
    pub start: u32,
    pub end: u32,
    /// "H1 > H2" path of the section the chunk belongs to.
    pub heading_path: String,
    /// Text that gets embedded (title + heading path + body slice).
    pub text: String,
    pub hash: u64,
}

const TARGET_WORDS: usize = 300;
const MAX_WORDS: usize = 400;

pub fn chunk_note(src: &str, note: &ParsedNote) -> Vec<Chunk> {
    let body_start = note.body_offset as usize;
    let mut bounds: Vec<(usize, usize)> = Vec::new(); // (offset, heading index)
    let mut path: Vec<(u8, String)> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    bounds.push((body_start, usize::MAX));
    paths.push(String::new());
    for (i, h) in note.headings.iter().enumerate() {
        while path.last().is_some_and(|(l, _)| *l >= h.level) {
            path.pop();
        }
        path.push((h.level, h.text.clone()));
        bounds.push((h.offset as usize, i));
        paths.push(path.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > "));
    }
    let mut out = Vec::new();
    for (k, &(start, _)) in bounds.iter().enumerate() {
        let end = bounds.get(k + 1).map(|b| b.0).unwrap_or(src.len());
        if end <= start || start > src.len() {
            continue;
        }
        let section = &src[start..end];
        let hp = &paths[k];
        for (s, e) in windows(section) {
            let slice = section[s..e].trim();
            if slice.split_whitespace().count() < 3 && !(k == 0 && out.is_empty() && !slice.is_empty()) {
                continue;
            }
            let text = if hp.is_empty() { format!("{}\n{}", note.title, slice) } else { format!("{} > {}\n{}", note.title, hp, slice) };
            let hash = xxhash_rust::xxh3::xxh3_64(text.as_bytes());
            out.push(Chunk { start: (start + s) as u32, end: (start + e) as u32, heading_path: hp.clone(), text, hash });
        }
    }
    if out.is_empty() {
        let text = note.title.clone();
        out.push(Chunk { start: 0, end: src.len() as u32, heading_path: String::new(), hash: xxhash_rust::xxh3::xxh3_64(text.as_bytes()), text });
    }
    out
}

/// Word windows over a section, returned as byte ranges. Windows break at sentence ends where possible
/// and the next window starts at the last sentence of the previous one (one-sentence overlap).
fn windows(s: &str) -> Vec<(usize, usize)> {
    let words: Vec<(usize, usize)> = word_spans(s);
    if words.len() <= MAX_WORDS {
        return vec![(0, s.len())];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let hard = (i + MAX_WORDS).min(words.len());
        let mut cut = hard;
        if hard < words.len() {
            // Prefer a sentence end between TARGET and MAX.
            for j in (i + TARGET_WORDS.min(hard - i)..hard).rev() {
                if ends_sentence(&s[words[j].0..words[j].1]) {
                    cut = j + 1;
                    break;
                }
            }
        }
        out.push((words[i].0, words[cut - 1].1));
        if cut >= words.len() {
            break;
        }
        // Overlap: back up to the start of the last sentence in this window (bounded).
        let mut back = cut - 1;
        while back > i + 1 && !ends_sentence(&s[words[back - 1].0..words[back - 1].1]) && cut - back < 40 {
            back -= 1;
        }
        i = if cut - back >= 40 || back <= i { cut } else { back };
    }
    out
}

fn ends_sentence(w: &str) -> bool {
    w.ends_with('.') || w.ends_with('!') || w.ends_with('?')
}

fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if let Some(st) = start.take() {
                v.push((st, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        v.push((st, s.len()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_by_heading_and_size() {
        let mut src = String::from("Intro words here today.\n\n# A\n\nshort section text here\n\n## B\n\n");
        for i in 0..900 {
            src.push_str(&format!("word{i}"));
            src.push_str(if i % 10 == 9 { ". " } else { " " });
        }
        let n = crate::parse("Note.md", &src);
        let c = chunk_note(&src, &n);
        assert_eq!(c[0].heading_path, "");
        assert_eq!(c[1].heading_path, "A");
        assert!(c.len() >= 5, "{}", c.len());
        assert!(c[2].heading_path == "A > B");
        for ch in &c[2..] {
            let words = src[ch.start as usize..ch.end as usize].split_whitespace().count();
            assert!(words <= MAX_WORDS + 5, "{words}");
        }
        // overlap: consecutive windows in the same section share text
        assert!(c[3].start < c[2].end);
    }
}
