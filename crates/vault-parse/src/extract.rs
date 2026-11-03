use crate::frontmatter;
use crate::{Heading, Link, LinkKind, ParsedNote, Subpath, Task};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

pub fn title_from_path(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    match name.rsplit_once('.') {
        Some((stem, ext)) if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") => stem.to_string(),
        _ => name.to_string(),
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_WIKILINKS
        | Options::ENABLE_MATH
}

/// Parse one note. `rel` is the normalized vault path (used for the title).
pub fn parse(rel: &str, src: &str) -> ParsedNote {
    let mut note = ParsedNote { title: title_from_path(rel), ..Default::default() };
    let (yaml, body_off) = frontmatter::split(src);
    note.body_offset = body_off as u32;
    let mut tags: Vec<String> = Vec::new();
    if let Some(y) = yaml {
        if let Some(fm) = frontmatter::parse_yaml(y) {
            let alias_v = fm.get("aliases").or_else(|| fm.get("alias"));
            note.aliases = frontmatter::string_list(alias_v, false);
            let tag_v = fm.get("tags").or_else(|| fm.get("tag"));
            for t in frontmatter::string_list(tag_v, true) {
                push_tag(&mut tags, t.trim_start_matches('#'));
            }
            note.frontmatter = Some(fm);
        }
    }
    let body = &src[body_off..];

    // Pass 1: pulldown-cmark for structure, code ranges, plain text, markdown links.
    let mut excluded: Vec<Range<usize>> = Vec::new();
    let mut text = String::with_capacity(body.len());
    let mut heading: Option<(u8, usize, String)> = None;
    let mut task_stack: Vec<Option<(bool, usize, String)>> = Vec::new();
    let mut md_link: Option<(String, usize)> = None;
    let mut in_wikilink = false;
    for (ev, range) in Parser::new_ext(body, options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                excluded.push(range.clone());
                if let CodeBlockKind::Fenced(_) = kind {
                    text.push('\n');
                }
            }
            Event::Start(Tag::HtmlBlock) | Event::Start(Tag::MetadataBlock(_)) => excluded.push(range.clone()),
            Event::Code(c) => {
                excluded.push(range.clone());
                text.push_str(&c);
            }
            Event::InlineHtml(_) | Event::Html(_) | Event::InlineMath(_) | Event::DisplayMath(_) => excluded.push(range.clone()),
            Event::Start(Tag::Heading { level, .. }) => heading = Some((heading_level(level), range.start, String::new())),
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, off, t)) = heading.take() {
                    note.headings.push(Heading { level, text: t.trim().to_string(), offset: (off + body_off) as u32 });
                }
                text.push('\n');
            }
            Event::Start(Tag::Item) => task_stack.push(None),
            Event::TaskListMarker(done) => {
                if let Some(top) = task_stack.last_mut() {
                    *top = Some((done, range.start, String::new()));
                }
            }
            Event::End(TagEnd::Item) => {
                if let Some(Some((done, off, t))) = task_stack.pop() {
                    note.tasks.push(Task { done, text: t.trim().to_string(), offset: (off + body_off) as u32 });
                }
                text.push('\n');
            }
            Event::Start(Tag::Link { link_type, dest_url, .. }) | Event::Start(Tag::Image { link_type, dest_url, .. }) => {
                if matches!(link_type, pulldown_cmark::LinkType::WikiLink { .. }) {
                    in_wikilink = true;
                } else if is_local_note_url(&dest_url) {
                    md_link = Some((dest_url.to_string(), range.start));
                }
                let _ = range;
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                if in_wikilink {
                    in_wikilink = false;
                } else if let Some((url, start)) = md_link.take() {
                    let (target, subpath) = split_subpath(&percent_decode(&url));
                    note.links.push(Link {
                        target: target.trim_start_matches("./").to_string(),
                        subpath,
                        display: None,
                        kind: LinkKind::Link,
                        start: (start + body_off) as u32,
                        end: (range.end + body_off) as u32,
                    });
                }
            }
            Event::Text(t) => {
                if let Some((_, _, h)) = heading.as_mut() {
                    h.push_str(&t);
                }
                if let Some(Some((_, _, tt))) = task_stack.last_mut() {
                    tt.push_str(&t);
                }
                text.push_str(&t);
            }
            Event::SoftBreak | Event::HardBreak => {
                text.push(' ');
                if let Some((_, _, h)) = heading.as_mut() {
                    h.push(' ');
                }
            }
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::TableCell) => text.push('\n'),
            _ => {}
        }
    }
    excluded.sort_by_key(|r| r.start);

    // Pass 2: raw scan for wikilinks, tags and block ids outside code.
    scan_raw(body, body_off, &excluded, &mut note, &mut tags);
    note.links.sort_by_key(|l| l.start);
    note.tags = tags;
    note.word_count = text.split_whitespace().count() as u32;
    note.body_text = text;
    note
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn is_local_note_url(u: &str) -> bool {
    !u.is_empty() && !u.contains("://") && !u.starts_with('#') && !u.starts_with("mailto:") && !u.starts_with("data:")
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn split_subpath(t: &str) -> (String, Option<Subpath>) {
    if let Some((a, b)) = t.split_once('#') {
        let b = b.trim();
        if let Some(block) = b.strip_prefix('^') {
            (a.trim().to_string(), Some(Subpath::Block(block.to_string())))
        } else if !b.is_empty() {
            // Obsidian allows [[note#h1#h2]]; keep the last heading.
            let last = b.rsplit('#').next().unwrap_or(b).trim().to_string();
            (a.trim().to_string(), Some(Subpath::Heading(last)))
        } else {
            (a.trim().to_string(), None)
        }
    } else if let Some((a, block)) = t.split_once('^') {
        // Spec form [[note^block]] (Obsidian writes [[note#^block]]); accept both.
        (a.trim().to_string(), Some(Subpath::Block(block.trim().to_string())))
    } else {
        (t.trim().to_string(), None)
    }
}

fn push_tag(tags: &mut Vec<String>, t: &str) {
    let t = t.trim().trim_end_matches('/').to_lowercase();
    if !t.is_empty() && !tags.contains(&t) {
        tags.push(t);
    }
}

fn is_tag_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '/'
}

fn scan_raw(body: &str, base: usize, excluded: &[Range<usize>], note: &mut ParsedNote, tags: &mut Vec<String>) {
    let bytes = body.as_bytes();
    let mut ex = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        while ex < excluded.len() && excluded[ex].end <= i {
            ex += 1;
        }
        if ex < excluded.len() && excluded[ex].start <= i {
            i = excluded[ex].end;
            continue;
        }
        let b = bytes[i];
        if b == b'[' && bytes.get(i + 1) == Some(&b'[') {
            let embed = i > 0 && bytes[i - 1] == b'!';
            if let Some(close) = find_close(bytes, i + 2) {
                let inner = &body[i + 2..close];
                if !inner.is_empty() && !inner.contains('\n') {
                    let (target_part, display) = match inner.split_once('|') {
                        Some((a, d)) => (a, Some(d.trim().to_string())),
                        None => (inner, None),
                    };
                    let (target, subpath) = split_subpath(target_part);
                    let start = if embed { i - 1 } else { i };
                    note.links.push(Link {
                        target,
                        subpath,
                        display,
                        kind: if embed { LinkKind::Embed } else { LinkKind::Link },
                        start: (start + base) as u32,
                        end: (close + 2 + base) as u32,
                    });
                    i = close + 2;
                    continue;
                }
            }
        } else if b == b'#' && (i == 0 || (bytes[i - 1] as char).is_whitespace() || bytes[i - 1] == b'(') {
            let rest = &body[i + 1..];
            let len: usize = rest.chars().take_while(|&c| is_tag_char(c)).map(char::len_utf8).sum();
            let tag = &rest[..len];
            if len > 0 && !tag.chars().all(|c| c.is_ascii_digit()) && !tag.starts_with('/') {
                push_tag(tags, tag);
                i += 1 + len;
                continue;
            }
        } else if b == b'^' && i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\n') {
            let rest = &body[i + 1..];
            let len = rest.bytes().take_while(|c| c.is_ascii_alphanumeric() || *c == b'-').count();
            let after = rest.as_bytes().get(len).copied();
            if len > 0 && matches!(after, None | Some(b'\n') | Some(b'\r')) {
                note.block_ids.push((rest[..len].to_string(), (i + base) as u32));
                i += 1 + len;
                continue;
            }
        }
        i += 1;
    }
}

fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < bytes.len() {
        match bytes[j] {
            b'\n' => return None,
            b']' if bytes[j + 1] == b']' => return Some(j),
            _ => j += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_everything() {
        let src = "---\naliases: [Alpha, \"The A\"]\ntags: [project, Work/Q1]\n---\n# Heading One\n\nSee [[Beta]] and [[Gamma#Intro|the intro]] and ![[pic.png]] #idea #123 not#tag.\n\n```\n[[NotALink]] #nottag\n```\n\n- [ ] todo `[[nope]]`\n- [x] done item ^blk-1\n\n[md](Delta%20Note.md#Sub) [web](https://x.y)\n";
        let n = parse("folder/Alpha Note.md", src);
        assert_eq!(n.title, "Alpha Note");
        assert_eq!(n.aliases, vec!["Alpha", "The A"]);
        assert_eq!(n.tags, vec!["project", "work/q1", "idea"]);
        assert_eq!(n.headings.len(), 1);
        assert_eq!(n.headings[0].text, "Heading One");
        let targets: Vec<_> = n.links.iter().map(|l| (l.target.as_str(), l.kind)).collect();
        assert_eq!(
            targets,
            vec![("Beta", LinkKind::Link), ("Gamma", LinkKind::Link), ("pic.png", LinkKind::Embed), ("Delta Note.md", LinkKind::Link)]
        );
        assert_eq!(n.links[1].subpath, Some(Subpath::Heading("Intro".into())));
        assert_eq!(n.links[1].display.as_deref(), Some("the intro"));
        assert_eq!(&src[n.links[0].start as usize..n.links[0].end as usize], "[[Beta]]");
        assert_eq!(&src[n.links[2].start as usize..n.links[2].end as usize], "![[pic.png]]");
        assert_eq!(n.tasks.len(), 2);
        assert!(!n.tasks[0].done && n.tasks[1].done);
        assert_eq!(n.block_ids, vec![("blk-1".to_string(), src.find("^blk-1").unwrap() as u32)]);
        assert!(n.body_text.contains("the intro"));
        assert!(!n.body_text.contains("aliases"));
    }

    #[test]
    fn block_link_and_bad_frontmatter() {
        let n = parse("x.md", "---\n: : bad\n---\n[[Note^abc]] [[#Local]]");
        assert!(n.frontmatter.is_none());
        assert_eq!(n.links[0].subpath, Some(Subpath::Block("abc".into())));
        assert_eq!(n.links[1].target, "");
    }

    #[test]
    fn unterminated_frontmatter_is_body() {
        let n = parse("x.md", "---\ntitle: x\n# Not closed");
        assert_eq!(n.body_offset, 0);
    }
}
