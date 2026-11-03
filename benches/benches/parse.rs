//! Micro-benchmarks for the parser (`cargo bench -p vault-benches --bench parse`).
fn main() {
    divan::main();
}

const NOTE: &str = include_str!("../../crates/vault-core/tests/fixtures/sample-vault/Home.md");

#[divan::bench]
fn parse_note() -> vault_parse::ParsedNote {
    vault_parse::parse("Home.md", divan::black_box(NOTE))
}

#[divan::bench]
fn parse_5kb(b: divan::Bencher) {
    let mut s = String::from("---\ntags: [a, b]\naliases: [X]\n---\n# Title\n\n");
    while s.len() < 5000 {
        s.push_str("Some words with a [[Link target]] and #tag plus **bold** text and more words. ");
        if s.len() % 700 < 80 {
            s.push_str("\n\n## Heading\n\n- [ ] task item\n");
        }
    }
    b.bench(|| vault_parse::parse("n.md", divan::black_box(&s)));
}
