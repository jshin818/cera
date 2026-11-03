//! `cargo run -p vault-benches --release -- <vault>`: per-stage parse profile over a vault.
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("vault path");
    let root = std::path::Path::new(&dir);
    let files: Vec<_> = vault_fs::walk(root, &Default::default()).into_iter().filter(|f| f.kind == vault_fs::FileKind::Markdown).take(5000).collect();
    let t = Instant::now();
    let srcs: Vec<(String, String)> = files.iter().map(|f| (f.rel.clone(), String::from_utf8(vault_fs::read(root, &f.rel).unwrap()).unwrap())).collect();
    let read = t.elapsed();
    let t = Instant::now();
    let parsed: Vec<_> = srcs.iter().map(|(r, s)| vault_parse::parse(r, s)).collect();
    let parse = t.elapsed();
    let t = Instant::now();
    let mh = vault_graph::MinHasher::default();
    for p in &parsed {
        std::hint::black_box(mh.signature(&p.body_text));
    }
    let minhash = t.elapsed();
    let t = Instant::now();
    for ((_, s), p) in srcs.iter().zip(&parsed) {
        std::hint::black_box(vault_parse::chunk_note(s, p));
    }
    let chunk = t.elapsed();
    let n = files.len() as f64;
    println!("{} files: read {:.1} µs, parse {:.1} µs, minhash {:.1} µs, chunk {:.1} µs per file (single thread)", n, read.as_micros() as f64 / n, parse.as_micros() as f64 / n, minhash.as_micros() as f64 / n, chunk.as_micros() as f64 / n);
}
