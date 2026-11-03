//! Generates a deterministic synthetic vault with realistic structure: topic folders (communities),
//! Zipf-distributed vocabulary, headings, wikilinks (mostly within a topic), aliases, nested tags,
//! frontmatter, tasks, code blocks and attachments (embeds). Average note size ~5 KB.

use anyhow::Result;
use clap::Parser;
use rand::prelude::*;
use rand::rngs::StdRng;
use rayon::prelude::*;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Generate a synthetic vault")]
struct Args {
    /// Output folder (created; must be empty or absent unless --force)
    out: PathBuf,
    /// Number of notes
    #[arg(long, default_value_t = 10_000)]
    notes: usize,
    /// Attachments as a fraction of notes
    #[arg(long, default_value_t = 0.1)]
    attachments: f64,
    /// Average note size in bytes
    #[arg(long, default_value_t = 5000)]
    avg_bytes: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long)]
    force: bool,
}

const SYL: &[&str] = &[
    "ka", "lo", "mi", "ra", "ten", "vo", "shi", "na", "per", "dor", "al", "en", "ti", "qua", "ber", "son", "ex", "lin", "mar", "ost", "rin", "ul", "zan", "fel",
    "gor", "hal", "ix", "jo", "kel", "mon", "nis", "ophe", "pra", "rus", "sel", "tor", "ur", "ven", "wy", "zel",
];
const COMMON: &[&str] = &[
    "the", "and", "of", "to", "in", "a", "is", "that", "for", "it", "as", "with", "was", "on", "be", "by", "this", "are", "from", "at", "or", "an", "which",
    "we", "can", "not", "but", "they", "more", "about", "into", "also", "how", "when", "use", "note", "idea", "work", "project", "time", "plan", "system",
    "design", "data", "review", "question", "result", "change", "team", "reading", "summary", "draft", "meeting", "goal", "process", "example", "model",
];
const TOPICS: &[&str] = &[
    "Engineering", "Research", "Reading", "Journal", "People", "Projects", "Cooking", "Travel", "Health", "Finance", "Music", "Philosophy", "History",
    "Writing", "Design", "Garden", "Physics", "Biology", "Language", "Teaching", "Startups", "Movies", "Games", "Fitness", "Photography", "Architecture",
    "Economics", "Psychology", "Math", "Ops", "Security", "Product", "Marketing", "Hiring", "Home", "Family", "Climbing", "Coffee", "Wine", "Art",
    "Poetry", "Chess", "Astronomy", "Geology", "Ocean", "Birds", "Cars", "Bikes", "Running", "Meditation",
];

fn word(rng: &mut StdRng, n_syl: usize) -> String {
    (0..n_syl).map(|_| SYL[rng.random_range(0..SYL.len())]).collect()
}

struct Topic {
    name: &'static str,
    subs: Vec<String>,
    vocab: Vec<String>,
}

/// Zipf-ish sampling: index ~ floor(n * u^2.2)
fn zipf<'a, T>(rng: &mut StdRng, v: &'a [T]) -> &'a T {
    let u: f64 = rng.random();
    &v[((v.len() as f64) * u.powf(2.2)) as usize % v.len()]
}

fn main() -> Result<()> {
    let a = Args::parse();
    if a.out.exists() && std::fs::read_dir(&a.out)?.next().is_some() && !a.force {
        anyhow::bail!("{} is not empty (use --force)", a.out.display());
    }
    std::fs::create_dir_all(&a.out)?;
    let mut rng = StdRng::seed_from_u64(a.seed);
    let n_topics = (a.notes / 2000).clamp(8, TOPICS.len());
    let topics: Vec<Topic> = TOPICS[..n_topics]
        .iter()
        .map(|&name| Topic {
            name,
            subs: {
                let n = rng.random_range(2..6);
                (0..n).map(|_| capitalize(&word(&mut rng, 2))).collect()
            },
            vocab: (0..400)
                .map(|_| {
                    let k = rng.random_range(1..4);
                    word(&mut rng, k)
                })
                .collect(),
        })
        .collect();
    let global: Vec<String> = (0..3000)
        .map(|_| {
            let k = rng.random_range(1..4);
            word(&mut rng, k)
        })
        .collect();

    // Titles (unique) and placement.
    let mut used = std::collections::HashSet::new();
    let mut notes: Vec<(usize, String, String)> = Vec::with_capacity(a.notes); // (topic, folder, title)
    for i in 0..a.notes {
        let t = (i * 7919 + rng.random_range(0..n_topics)) % n_topics;
        let tp = &topics[t];
        let folder = if rng.random_bool(0.7) { format!("{}/{}", tp.name, tp.subs[rng.random_range(0..tp.subs.len())]) } else { tp.name.to_string() };
        let title = loop {
            let k = rng.random_range(1..4);
            let mut s: Vec<String> = (0..k).map(|_| zipf(&mut rng, &tp.vocab).clone()).collect();
            if rng.random_bool(0.3) {
                s.push(zipf(&mut rng, &global).clone());
            }
            let mut t = capitalize(&s.join(" "));
            if !used.insert(t.to_lowercase()) {
                t = format!("{t} {}", rng.random_range(2..9999));
                if !used.insert(t.to_lowercase()) {
                    continue;
                }
            }
            break t;
        };
        notes.push((t, folder, title));
    }
    let by_topic: Vec<Vec<usize>> = (0..n_topics).map(|t| notes.iter().enumerate().filter(|(_, n)| n.0 == t).map(|(i, _)| i).collect()).collect();
    let n_att = (a.notes as f64 * a.attachments) as usize;
    let attachments: Vec<String> = (0..n_att).map(|i| format!("{}/attachments/img-{i:06}.png", topics[i % n_topics].name)).collect();
    for p in &attachments {
        let full = a.out.join(p);
        std::fs::create_dir_all(full.parent().unwrap())?;
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend((0..200 + (p.len() * 31) % 600).map(|i| (i * 7 + p.len()) as u8));
        std::fs::write(full, bytes)?;
    }
    let mut dirs: Vec<&String> = notes.iter().map(|n| &n.1).collect();
    dirs.sort();
    dirs.dedup();
    for d in dirs {
        std::fs::create_dir_all(a.out.join(d))?;
    }

    let seed = a.seed;
    let total: usize = notes
        .par_iter()
        .enumerate()
        .map(|(i, (t, folder, title))| {
            let mut rng = StdRng::seed_from_u64(seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let tp = &topics[*t];
            let mut s = String::with_capacity(a.avg_bytes + 512);
            let tags: Vec<String> = (0..rng.random_range(0..4)).map(|_| if rng.random_bool(0.5) { format!("{}/{}", tp.name.to_lowercase(), tp.subs[rng.random_range(0..tp.subs.len())].to_lowercase()) } else { tp.name.to_lowercase() }).collect();
            if rng.random_bool(0.6) {
                s.push_str("---\n");
                if !tags.is_empty() {
                    let _ = writeln!(s, "tags: [{}]", tags.join(", "));
                }
                if rng.random_bool(0.15) {
                    let _ = writeln!(s, "aliases: [{}]", capitalize(&word(&mut rng, 3)));
                }
                let _ = writeln!(s, "created: 2026-{:02}-{:02}", rng.random_range(1..13), rng.random_range(1..29));
                if rng.random_bool(0.3) {
                    let _ = writeln!(s, "status: {}", ["draft", "active", "done"][rng.random_range(0..3)]);
                }
                s.push_str("---\n");
            }
            let _ = writeln!(s, "# {title}\n");
            let target = (a.avg_bytes as f64 * rng.random_range(0.3..1.7)) as usize;
            let mut section = 0;
            while s.len() < target {
                if rng.random_bool(0.25) {
                    section += 1;
                    let _ = writeln!(s, "{} {}\n", if rng.random_bool(0.7) { "##" } else { "###" }, capitalize(&format!("{} {}", zipf(&mut rng, &tp.vocab), zipf(&mut rng, &global))));
                }
                // paragraph
                let words = rng.random_range(30..110);
                let mut para = String::new();
                for w in 0..words {
                    let r: f64 = rng.random();
                    let tok = if r < 0.045 {
                        // link: 80% same topic
                        let j = if rng.random_bool(0.8) && !by_topic[*t].is_empty() { by_topic[*t][rng.random_range(0..by_topic[*t].len())] } else { rng.random_range(0..notes.len()) };
                        let tgt = &notes[j].2;
                        match rng.random_range(0..10) {
                            0 => format!("[[{tgt}|{}]]", zipf(&mut rng, &tp.vocab)),
                            1 => format!("[[{tgt}#Overview]]"),
                            2 if rng.random_bool(0.3) => format!("[[Missing {}]]", word(&mut rng, 2)),
                            _ => format!("[[{tgt}]]"),
                        }
                    } else if r < 0.05 && !tags.is_empty() {
                        format!("#{}", tags[0])
                    } else if r < 0.45 {
                        zipf(&mut rng, COMMON).to_string()
                    } else if r < 0.80 {
                        zipf(&mut rng, &tp.vocab).clone()
                    } else {
                        zipf(&mut rng, &global).clone()
                    };
                    if w > 0 {
                        para.push(' ');
                    }
                    para.push_str(&tok);
                    if w > 0 && w % rng.random_range(9..18) == 0 {
                        para.push('.');
                    }
                }
                para.push_str(".\n\n");
                s.push_str(&capitalize(&para));
                let r: f64 = rng.random();
                if r < 0.08 {
                    for _ in 0..rng.random_range(2..6) {
                        let _ = writeln!(s, "- [{}] {} {}", if rng.random_bool(0.4) { "x" } else { " " }, zipf(&mut rng, COMMON), zipf(&mut rng, &tp.vocab));
                    }
                    s.push('\n');
                } else if r < 0.11 {
                    let _ = writeln!(s, "```rust\nfn {}() {{ println!(\"[[not a link]] #nottag\"); }}\n```\n", word(&mut rng, 2));
                } else if r < 0.13 && !attachments.is_empty() {
                    let at = &attachments[rng.random_range(0..attachments.len())];
                    let _ = writeln!(s, "![[{}]]\n", at.rsplit('/').next().unwrap());
                } else if r < 0.16 {
                    let _ = writeln!(s, "> {} {} {}\n", capitalize(zipf(&mut rng, COMMON)), zipf(&mut rng, &tp.vocab), zipf(&mut rng, &global));
                }
                let _ = section;
            }
            let path = a.out.join(folder).join(format!("{title}.md"));
            std::fs::write(&path, &s).expect("write note");
            s.len()
        })
        .sum();
    // Query set for benchmarks: realistic mixes of title words, topic vocab, phrases, filters.
    let mut q = String::new();
    for i in 0..200 {
        let (t, _, title) = &notes[rng.random_range(0..notes.len())];
        let tp = &topics[*t];
        let line = match i % 8 {
            0 => title.to_lowercase(),
            1 => format!("{} {}", zipf(&mut rng, &tp.vocab), zipf(&mut rng, &tp.vocab)),
            2 => format!("\"{} {}\"", zipf(&mut rng, COMMON), zipf(&mut rng, &tp.vocab)),
            3 => format!("{} tag:{}", zipf(&mut rng, &tp.vocab), tp.name.to_lowercase()),
            4 => format!("{} path:{}", zipf(&mut rng, &tp.vocab), tp.name),
            5 => format!("{} -{}", zipf(&mut rng, &tp.vocab), zipf(&mut rng, &tp.vocab)),
            6 => title.split(' ').next().unwrap_or("x").to_lowercase(),
            _ => format!("{} {} {}", zipf(&mut rng, &global), zipf(&mut rng, &tp.vocab), zipf(&mut rng, COMMON)),
        };
        let _ = writeln!(q, "{line}");
    }
    std::fs::write(a.out.join(".vault-bench-queries.txt"), q)?;
    eprintln!("wrote {} notes ({:.1} MB, avg {} B) and {} attachments to {}", notes.len(), total as f64 / 1e6, total / notes.len().max(1), n_att, a.out.display());
    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
