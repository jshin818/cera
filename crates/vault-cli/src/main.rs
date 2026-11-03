//! `vault` — headless CLI over vault-core (Phase 0 deliverable) plus `serve` for the desktop UI.

mod bench;


use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use vault_core::{Engine, EngineOptions};

#[derive(Parser)]
#[command(name = "vault", version, about = "Fast, local-first Markdown notes engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Print JSON instead of text
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index (or refresh) a vault and print stats
    Index { vault: PathBuf, /// Also compute embeddings and graph metrics before exiting
        #[arg(long)] full: bool },
    /// Hybrid search (BM25 + vectors + RRF). Use --text for BM25 only
    Search { vault: PathBuf, query: String, #[arg(long)] text: bool, #[arg(short, long, default_value_t = 10)] limit: usize },
    /// Quick switcher
    Switch { vault: PathBuf, query: String, #[arg(short, long, default_value_t = 10)] limit: usize },
    /// Notes linking to a note (path relative to the vault)
    Backlinks { vault: PathBuf, note: String },
    /// Related notes and link suggestions
    Related { vault: PathBuf, note: String, #[arg(short, long, default_value_t = 10)] limit: usize },
    /// Unlinked mentions of a note
    Mentions { vault: PathBuf, note: String },
    /// Near-duplicate notes in the vault
    Dupes { vault: PathBuf },
    /// Leiden topic clusters
    Clusters { vault: PathBuf },
    /// Measure the Section 8 performance budgets on a vault
    Bench(bench::BenchArgs),
    /// Serve the desktop UI on localhost
    Serve {
        vault: PathBuf,
        #[arg(long, default_value = "127.0.0.1:4317")]
        addr: String,
        /// Open the default browser
        #[arg(long)]
        open: bool,
    },
    #[command(hide = true)]
    FirstFrame { vault: PathBuf },
    #[command(hide = true)]
    IdleRss { vault: PathBuf, #[arg(long)] no_embed: bool },
}

fn engine(vault: &PathBuf, embed: bool) -> Result<Engine> {
    let e = Engine::open(vault, EngineOptions { watch: false, embed, ..Default::default() }).with_context(|| format!("opening {}", vault.display()))?;
    e.wait_ready(Duration::from_secs(600));
    e.flush();
    Ok(e)
}

fn main() -> Result<()> {
    let start = Instant::now();
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_env("VAULT_LOG").unwrap_or_else(|_| "warn".into())).with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let json = cli.json;
    let print = |v: serde_json::Value, text: &dyn Fn(&serde_json::Value)| {
        if json {
            println!("{}", serde_json::to_string_pretty(&v).unwrap());
        } else {
            text(&v);
        }
    };
    match cli.cmd {
        Cmd::FirstFrame { vault } => {
            // Process start -> last note text in memory (the engine opens on the side).
            let (path, content, _) = vault_core::first_frame(&vault).context("no workspace.json with a last note")?;
            let e = Engine::open(&vault, EngineOptions { watch: false, embed: false, ..Default::default() })?;
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            println!("{}", serde_json::json!({"first_frame_ms": ms, "path": path, "bytes": content.len(), "open": e.timings()}));
            std::process::exit(0); // skip orderly shutdown: we only measure startup
        }
        Cmd::IdleRss { vault, no_embed } => {
            let e = Engine::open(&vault, EngineOptions { watch: true, embed: !no_embed, ..Default::default() })?;
            e.wait_ready(Duration::from_secs(1800));
            eprintln!("ready rss {:.0}", vault_core::rss_mb());
            e.wait_embeddings(Duration::from_secs(1800));
            e.flush();
            eprintln!("embedded rss {:.0}", vault_core::rss_mb());
            e.recompute_graph();
            eprintln!("graph rss {:.0}", vault_core::rss_mb());
            std::thread::sleep(Duration::from_secs(4));
            let (anon, file) = bench::rss_parts();
            println!("{}", serde_json::json!({"rss_mb": bench::rss_mb(), "anon_mb": anon, "file_mb": file}));
            std::process::exit(0);
        }
        Cmd::Index { vault, full } => {
            let e = engine(&vault, full)?;
            if full {
                e.wait_embeddings(Duration::from_secs(1800));
                e.recompute_graph();
            }
            e.persist()?;
            let s = e.status();
            print(serde_json::to_value(&s)?, &|_| {
                println!("{} notes, {} attachments, {} folders", s.notes, s.attachments, s.folders);
                let t = &s.timings;
                println!("open {:.1} ms (snapshot map {:.2} ms), hydrate {:.1} ms, walk {:.1} ms, index {:.1} ms ({} files parsed)", t.open_total_ms, t.snapshot_map_ms, t.hydrate_ms, t.walk_ms, t.index_ms, t.files_reparsed);
                if full {
                    println!("embedded {} chunks in {:.1} ms with {}; graph metrics {:.1} ms; {} clusters", t.embed_chunks, t.embed_ms, s.model, t.graph_ms, s.clusters);
                }
            });
            e.shutdown();
        }
        Cmd::Search { vault, query, text, limit } => {
            let e = engine(&vault, !text)?;
            if !text {
                e.wait_embeddings(Duration::from_secs(1800));
            }
            let t = Instant::now();
            let r = if text { e.search_text(&query, limit)? } else { e.search(&query, limit)? };
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            print(serde_json::to_value(&r)?, &|_| {
                for h in &r {
                    println!("{:7.4}  {}", h.score, h.note.path);
                    if !h.snippet.is_empty() {
                        println!("         {}", strip_tags(&h.snippet));
                    }
                }
                eprintln!("{} results in {ms:.2} ms", r.len());
            });
            e.shutdown();
        }
        Cmd::Switch { vault, query, limit } => {
            let e = engine(&vault, false)?;
            let t = Instant::now();
            let r = e.switch(&query, limit);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            print(serde_json::to_value(&r)?, &|_| {
                for h in &r {
                    println!("{}{}{}", h.note.path, h.alias.as_ref().map(|a| format!("  (alias: {a})")).unwrap_or_default(), if h.typo { "  ~" } else { "" });
                }
                eprintln!("{} results in {ms:.2} ms", r.len());
            });
            e.shutdown();
        }
        Cmd::Backlinks { vault, note } => {
            let e = engine(&vault, false)?;
            let r = e.backlinks(&note)?;
            print(serde_json::to_value(&r)?, &|_| {
                for c in &r {
                    println!("{}\n    {}", c.note.path, c.line);
                }
            });
            e.shutdown();
        }
        Cmd::Related { vault, note, limit } => {
            let e = engine(&vault, true)?;
            e.wait_embeddings(Duration::from_secs(1800));
            e.recompute_graph();
            let r = e.related(&note, limit)?;
            let s = e.link_suggestions(&note, limit)?;
            let tags = e.tag_suggestions(&note, 5)?;
            print(serde_json::json!({"related": r, "link_suggestions": s, "tag_suggestions": tags}), &|_| {
                println!("Related:");
                for x in &r {
                    println!("  {:.4}  {}  [{}]", x.score, x.note.path, x.why.join(", "));
                }
                println!("Suggested links:");
                for x in &s {
                    println!("  {}  ({})", x.note.path, x.reason);
                }
                println!("Suggested tags: {}", tags.iter().map(|(t, s)| format!("#{t} ({s:.2})")).collect::<Vec<_>>().join(", "));
            });
            e.shutdown();
        }
        Cmd::Mentions { vault, note } => {
            let e = engine(&vault, false)?;
            let r = e.unlinked_mentions(&note, 100)?;
            print(serde_json::to_value(&r)?, &|_| {
                for c in &r {
                    println!("{}\n    {}", c.note.path, c.line);
                }
            });
            e.shutdown();
        }
        Cmd::Dupes { vault } => {
            let e = engine(&vault, false)?;
            let r = e.all_near_duplicates();
            print(serde_json::to_value(&r)?, &|_| {
                for (a, b, j) in &r {
                    println!("{j:.2}  {}  <->  {}", a.path, b.path);
                }
            });
            e.shutdown();
        }
        Cmd::Clusters { vault } => {
            let e = engine(&vault, false)?;
            e.recompute_graph();
            let r = e.clusters(3);
            print(serde_json::to_value(&r)?, &|_| {
                for c in r.iter().take(40) {
                    println!("{:5}  {}  {}", c.size, c.label, c.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
                }
            });
            e.shutdown();
        }
        Cmd::Bench(args) => bench::run(args)?,
        Cmd::Serve { vault, addr, open } => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            rt.block_on(vault_server::serve(vault_server::ServeOptions { vault, addr, open_browser: open, token: None, on_ready: None }))?;
        }
    }
    Ok(())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
}
