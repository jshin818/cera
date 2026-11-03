//! `vault bench <vault>`: measures the Section 8 budgets and prints PASS/FAIL per row.

use anyhow::{Context, Result};
use rand::prelude::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use vault_core::{Engine, EngineOptions};

#[derive(clap::Args)]
pub struct BenchArgs {
    pub vault: PathBuf,
    /// Skip the fresh first-index measurement (it deletes and rebuilds .vault/)
    #[arg(long)]
    pub no_fresh: bool,
    /// Skip embeddings (and hybrid/related rows)
    #[arg(long)]
    pub no_embed: bool,
    /// Write results as JSON to this file
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Compare p95s with a previous `--out` file; exit non-zero on a >10% regression (CI gate)
    #[arg(long)]
    pub baseline: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Row {
    name: String,
    budget_ms: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    n: usize,
    pass: bool,
    note: String,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn row(name: &'static str, budget_ms: f64, mut samples: Vec<f64>, note: impl Into<String>) -> Row {
    let n = samples.len();
    let (p50, p95, p99) = (pct(&mut samples, 0.5), pct(&mut samples, 0.95), pct(&mut samples, 0.99));
    Row { name: name.to_string(), budget_ms, p50, p95, p99, n, pass: p95 <= budget_ms, note: note.into() }
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

/// (anonymous, file-backed) resident MB on Linux. File-backed pages are clean mmapped index data
/// the OS can drop at will; macOS's "memory footprint" likewise excludes them.
pub fn rss_parts() -> (f64, f64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let get = |k: &str| s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()).unwrap_or(f64::NAN) / 1024.0;
    (get("RssAnon:"), get("RssFile:"))
}

pub fn rss_mb() -> f64 {
    if let Ok(s) = std::fs::read_to_string("/proc/self/status")
        && let Some(l) = s.lines().find(|l| l.starts_with("VmRSS:"))
    {
        return l.split_whitespace().nth(1).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) / 1024.0;
    }
    // macOS
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}

fn first_frame_subprocess(vault: &PathBuf) -> Result<f64> {
    let exe = std::env::current_exe()?;
    let t = Instant::now();
    let out = std::process::Command::new(exe).arg("first-frame").arg(vault).output()?;
    let wall = t.elapsed().as_secs_f64() * 1000.0;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).with_context(|| String::from_utf8_lossy(&out.stderr).to_string())?;
    // Prefer the in-process number (excludes exec + dyld); report wall too.
    let inproc = v["first_frame_ms"].as_f64().unwrap_or(wall);
    eprintln!("    first-frame subprocess: in-process {inproc:.1} ms, wall incl. exec {wall:.1} ms");
    Ok(wall)
}

pub fn run(a: BenchArgs) -> Result<()> {
    let vault = a.vault.canonicalize()?;
    let mut rows: Vec<Row> = Vec::new();
    let embed = !a.no_embed;
    let opts = |watch: bool| EngineOptions { watch, embed, ..Default::default() };
    eprintln!("Vault bench on {} ({} logical CPUs)", vault.display(), std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));

    // 1. Fresh first index.
    if !a.no_fresh {
        let _ = std::fs::remove_dir_all(vault.join(".vault"));
        eprintln!("  first index (fresh .vault/)...");
        let e = Engine::open(&vault, EngineOptions { watch: false, embed, ..Default::default() })?;
        let (_, ms) = time(|| e.wait_ready(Duration::from_secs(1800)));
        let t = e.timings();
        rows.push(row(
            "First index of new vault (text)",
            10_000.0,
            vec![t.ready_ms.max(ms)],
            format!("walk {:.0} ms, parse+index {:.0} ms, {} files", t.walk_ms, t.index_ms, t.files_reparsed),
        ));
        if embed {
            eprintln!("  first embedding pass...");
            let (_, ms) = time(|| e.wait_embeddings(Duration::from_secs(1800)));
            let t = e.timings();
            rows.push(row("First embedding pass", 30_000.0, vec![ms], format!("{} chunks, model {}", t.embed_chunks, e.status().model)));
        }
        e.flush();
        e.recompute_graph();
        e.persist()?;
        e.shutdown();
        drop(e);
    }

    // Make sure a last note exists for the first-frame path.
    {
        let e = Engine::open(&vault, EngineOptions { watch: false, embed: false, ..Default::default() })?;
        e.wait_ready(Duration::from_secs(1800));
        let mut ws = e.workspace();
        if ws.get("last").and_then(|x| x.as_str()).is_none() {
            let p = e.recent(1).first().map(|n| n.path.clone()).unwrap_or_default();
            ws["last"] = serde_json::json!(p);
            e.set_workspace(&ws)?;
        }
        e.persist()?;
        e.shutdown();
    }

    // 2. Cold start (first launch after a drop of OS caches is not possible unprivileged; this is a
    //    fresh process) and warm start (immediate second launch).
    eprintln!("  cold / warm start...");
    let cold = first_frame_subprocess(&vault)?;
    let warm: Vec<f64> = (0..5).map(|_| first_frame_subprocess(&vault)).collect::<Result<_>>()?;
    rows.push(row("Cold start -> last note in memory", 150.0, vec![cold], "fresh process; UI paint not included"));
    rows.push(row("Warm start", 50.0, warm, "fresh process, OS cache warm"));

    // 3. Open the engine for the query benchmarks.
    let e = Engine::open(&vault, opts(true))?;
    let t = e.timings();
    rows.push(row("Snapshot load (mmap + verify)", 15.0, vec![t.snapshot_map_ms], format!("{:.1} MB snapshot", t.snapshot_bytes as f64 / 1e6)));
    e.wait_ready(Duration::from_secs(1800));
    let t = e.timings();
    rows.push(row("Hydrate file table (switcher/sidebar usable)", 300.0, vec![t.hydrate_core_ms], "background, after first frame"));
    rows.push(row("Hydrate links + backlinks", 1000.0, vec![t.hydrate_ms], "background, after first frame (no spec budget)"));
    if embed {
        e.wait_embeddings(Duration::from_secs(1800));
    }
    e.flush();
    // Interactive latencies are measured on a settled engine (background jobs finished).
    eprintln!("  waiting for background work to settle...");
    e.wait_idle(Duration::from_secs(600));
    let snap = e.snapshot();
    let paths: Vec<String> = snap.notes().map(|n| n.path.clone()).collect();
    let titles: Vec<String> = snap.notes().map(|n| n.title.clone()).collect();
    let mut rng = StdRng::seed_from_u64(7);

    // 4. Quick switcher: random 1-6 char queries, typed one char at a time.
    eprintln!("  quick switcher...");
    let mut sw = Vec::new();
    for _ in 0..200 {
        let t = &titles[rng.random_range(0..titles.len())].to_lowercase();
        let chars: Vec<char> = t.chars().filter(|c| !c.is_whitespace()).collect();
        let n = rng.random_range(1..=6).min(chars.len().max(1));
        let start = rng.random_range(0..chars.len().saturating_sub(n).max(1));
        let q: String = chars[start..(start + n).min(chars.len())].iter().collect();
        // Type it one character at a time, as a user would; each keystroke is one sample.
        let mut typed = String::new();
        for c in q.chars() {
            typed.push(c);
            let (_, ms) = time(|| e.switch(&typed, 50));
            sw.push(ms);
        }
    }
    rows.push(row("Quick switcher per keystroke", 5.0, sw, format!("{} titles, typed 1-6 chars", titles.len())));

    // 5. Full-text and hybrid search on the query set.
    let queries: Vec<String> = std::fs::read_to_string(vault.join(".vault-bench-queries.txt"))
        .map(|s| s.lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect())
        .unwrap_or_else(|_| titles.iter().take(200).map(|t| t.to_lowercase()).collect());
    eprintln!("  full-text search ({} queries)...", queries.len());
    let _ = e.search_text(&queries[0], 20);
    let ft: Vec<f64> = queries.iter().map(|q| time(|| e.search_text(q, 20)).1).collect();
    rows.push(row("Full-text search", 30.0, ft, "BM25F with snippets, top 20"));
    if embed {
        eprintln!("  hybrid search...");
        let hy: Vec<f64> = queries.iter().map(|q| time(|| e.search(q, 20)).1).collect();
        rows.push(row("Hybrid search (BM25 + vector + RRF)", 50.0, hy, "excludes first model load"));
        e.recompute_graph();
        eprintln!("  related notes...");
        let sample: Vec<&String> = (0..100).map(|_| &paths[rng.random_range(0..paths.len())]).collect();
        let unc: Vec<f64> = sample.iter().map(|p| time(|| e.related(p, 10)).1).collect();
        let cached: Vec<f64> = sample.iter().map(|p| time(|| e.related(p, 10)).1).collect();
        rows.push(row("Related notes panel (uncached)", 30.0, unc, ""));
        rows.push(row("Related notes panel (cached)", 30.0, cached, ""));
    }

    // 6. Open / switch note (engine side: read file + metadata).
    let op: Vec<f64> = (0..200).map(|_| time(|| e.read_note(&paths[rng.random_range(0..paths.len())])).1).collect();
    rows.push(row("Open / switch note (engine side)", 16.0, op, "read + metadata; UI render not included"));

    // 7. External edit -> visible in search and backlinks.
    eprintln!("  external edits...");
    std::thread::sleep(Duration::from_millis(300));
    let mut ext = Vec::new();
    for i in 0..20 {
        let target = &titles[rng.random_range(0..titles.len())];
        let word = format!("zqx{}bench{}", i, rng.random_range(0..1_000_000));
        let rel = format!("bench-external-{i}.md");
        let t = Instant::now();
        std::fs::write(vault.join(&rel), format!("# External {i}\n\n{word} links to [[{target}]]\n"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let s = e.snapshot();
            let back_ok = s
                .by_path(&rel)
                .is_some_and(|n| n.links.first().and_then(|l| l.dst()).and_then(|d| s.get(d)).is_some_and(|d| s.backlinks(d.id).contains(&n.id)));
            if back_ok && !e.search_text(&word, 1).unwrap_or_default().is_empty() {
                ext.push(t.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            if Instant::now() > deadline {
                ext.push(5000.0);
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    for i in 0..20 {
        let _ = std::fs::remove_file(vault.join(format!("bench-external-{i}.md")));
    }
    rows.push(row("External edit -> visible in search/backlinks", 300.0, ext, "watcher debounce 100 ms + quiet-window commit"));

    e.shutdown();
    drop(e);
    // 8. Idle memory, in a fresh process (this one has allocator high-water marks from the benches).
    eprintln!("  idle memory...");
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("idle-rss").arg(&vault);
    if !embed {
        cmd.arg("--no-embed");
    }
    let out = cmd.output()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let anon = v["anon_mb"].as_f64().unwrap_or(f64::NAN);
    let total = v["rss_mb"].as_f64().unwrap_or(f64::NAN);
    rows.push(row(
        "Idle memory (MB, anonymous)",
        300.0,
        vec![anon],
        format!("fresh process after indexing settles; total RSS incl. clean mmapped index pages {total:.0} MB"),
    ));

    println!("\n{:<46} {:>9} {:>9} {:>9} {:>9}  result", "Interaction", "budget", "p50", "p95", "p99");
    for r in &rows {
        println!("{:<46} {:>9.1} {:>9.2} {:>9.2} {:>9.2}  {}  {}", r.name, r.budget_ms, r.p50, r.p95, r.p99, if r.pass { "PASS" } else { "FAIL" }, r.note);
    }
    if let Some(p) = a.out {
        std::fs::write(p, serde_json::to_vec_pretty(&rows)?)?;
    }
    if let Some(b) = a.baseline {
        let base: Vec<Row> = serde_json::from_slice(&std::fs::read(&b)?)?;
        let mut regressions = Vec::new();
        for r in &rows {
            if let Some(o) = base.iter().find(|o| o.name == r.name) {
                // Ignore sub-millisecond noise; flag >10% p95 regressions.
                if r.p95 > o.p95 * 1.10 && r.p95 - o.p95 > 1.0 {
                    regressions.push(format!("{}: p95 {:.2} -> {:.2} ms", r.name, o.p95, r.p95));
                }
            }
        }
        if !regressions.is_empty() {
            eprintln!("\nRegressions over 10% (justify in docs/decisions.md or fix):");
            for r in &regressions {
                eprintln!("  {r}");
            }
            std::process::exit(2);
        }
        eprintln!("\nNo regressions over 10% against {}", b.display());
    }
    Ok(())
}
