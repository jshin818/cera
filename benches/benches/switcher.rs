//! Quick switcher scoring over 100k synthetic titles (`cargo bench -p vault-benches --bench switcher`).
use vault_search::{SwitchItem, Switcher};

fn main() {
    divan::main();
}

fn items(n: usize) -> Vec<SwitchItem> {
    let syl = ["ka", "lo", "mi", "ra", "ten", "vo", "shi", "na", "per", "dor", "al", "en"];
    (0..n)
        .map(|i| {
            let t: String = (0..3).map(|k| syl[(i * 7 + k * 13 + i / 5) % syl.len()]).collect::<Vec<_>>().join(" ") + &format!(" {i}");
            SwitchItem { id: i as u32, title: t.clone(), aliases: vec![], path: format!("f{}/{t}.md", i % 50) }
        })
        .collect()
}

#[divan::bench(args = ["k", "ka", "kalo", "kalomi"])]
fn query_100k(b: divan::Bencher, q: &str) {
    let s = Switcher::new(items(100_000), |_| 0.0, |_| 0.0);
    b.bench(|| {
        // fresh query each time (no incremental narrowing)
        s.query("zz", 1);
        s.query(divan::black_box(q), 50)
    });
}
