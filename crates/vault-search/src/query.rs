//! Query syntax: plain terms, "phrases", tag:x, path:x, title:x, and -exclusions of any of those.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    Term(String),
    Phrase(Vec<String>),
    Tag(String),
    Path(String),
    Title(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    pub must: Vec<Clause>,
    pub exclude: Vec<Clause>,
    /// Whether the input ends mid-word (as-you-type prefix boost applies to the last term).
    pub trailing_word: bool,
}

impl ParsedQuery {
    /// Free-text words (terms and phrase words), used for embeddings and highlighting.
    pub fn text(&self) -> String {
        let mut out = Vec::new();
        for c in &self.must {
            match c {
                Clause::Term(t) | Clause::Title(t) => out.push(t.clone()),
                Clause::Phrase(p) => out.extend(p.iter().cloned()),
                _ => {}
            }
        }
        out.join(" ")
    }
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| w.to_lowercase()).collect()
}

pub fn parse_query(q: &str) -> ParsedQuery {
    let mut out = ParsedQuery { trailing_word: q.chars().last().is_some_and(|c| c.is_alphanumeric()), ..Default::default() };
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let neg = chars[i] == '-' && chars.get(i + 1).is_some_and(|c| !c.is_whitespace());
        if neg {
            i += 1;
        }
        let tok_start = i;
        let mut prefix: Option<String> = None;
        // field prefix?
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j < chars.len() && chars[j] == ':' && j > i {
            let f: String = chars[i..j].iter().collect::<String>().to_lowercase();
            if matches!(f.as_str(), "tag" | "path" | "title") {
                prefix = Some(f);
                i = j + 1;
            }
        }
        let value: String = if chars.get(i) == Some(&'"') {
            let s = i + 1;
            let mut e = s;
            while e < chars.len() && chars[e] != '"' {
                e += 1;
            }
            i = (e + 1).min(chars.len());
            let v: String = chars[s..e.min(chars.len())].iter().collect();
            if prefix.is_none() {
                let w = words(&v);
                if !w.is_empty() {
                    let c = if w.len() == 1 { Clause::Term(w[0].clone()) } else { Clause::Phrase(w) };
                    if neg { out.exclude.push(c) } else { out.must.push(c) }
                }
                continue;
            }
            v
        } else {
            let s = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            chars[s..i].iter().collect()
        };
        let _ = tok_start;
        let clause = match prefix.as_deref() {
            Some("tag") => {
                let t = value.trim_start_matches('#').trim_end_matches('/').to_lowercase();
                (!t.is_empty()).then_some(Clause::Tag(t))
            }
            Some("path") => {
                let p = value.trim_matches('/').to_lowercase();
                (!p.is_empty()).then_some(Clause::Path(p))
            }
            Some("title") => {
                let w = words(&value);
                (!w.is_empty()).then(|| Clause::Title(w.join(" ")))
            }
            _ => {
                if let Some(t) = value.strip_prefix('#').filter(|t| !t.is_empty() && !t.starts_with('#')) {
                    Some(Clause::Tag(t.to_lowercase()))
                } else {
                    let w = words(&value);
                    match w.len() {
                        0 => None,
                        1 => Some(Clause::Term(w[0].clone())),
                        _ => {
                            for x in &w[..w.len() - 1] {
                                if neg { out.exclude.push(Clause::Term(x.clone())) } else { out.must.push(Clause::Term(x.clone())) }
                            }
                            Some(Clause::Term(w[w.len() - 1].clone()))
                        }
                    }
                }
            }
        };
        if let Some(c) = clause {
            if neg { out.exclude.push(c) } else { out.must.push(c) }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_syntax() {
        let q = parse_query(r#"rust "zero copy" tag:Lang path:Projects/ -draft -tag:old title:Plan #idea"#);
        assert_eq!(
            q.must,
            vec![
                Clause::Term("rust".into()),
                Clause::Phrase(vec!["zero".into(), "copy".into()]),
                Clause::Tag("lang".into()),
                Clause::Path("projects".into()),
                Clause::Title("plan".into()),
                Clause::Tag("idea".into()),
            ]
        );
        assert_eq!(q.exclude, vec![Clause::Term("draft".into()), Clause::Tag("old".into())]);
        assert!(q.trailing_word);
        assert!(!parse_query("a b ").trailing_word);
        assert_eq!(parse_query("well-known").must.len(), 2);
    }
}
