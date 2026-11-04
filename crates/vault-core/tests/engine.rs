//! Golden and behavioural tests against the sample Obsidian vault in tests/fixtures.

use std::path::Path;
use std::time::{Duration, Instant};
use vault_core::{Engine, EngineOptions};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        if p.is_dir() {
            copy_dir(&p, &to.join(e.file_name()));
        } else {
            std::fs::copy(&p, to.join(e.file_name())).unwrap();
        }
    }
}

fn sample() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-vault"), d.path());
    d
}

fn opts(watch: bool) -> EngineOptions {
    EngineOptions { watch, models_dir: None, idle: Duration::from_millis(200), ..Default::default() }
}

fn open(dir: &Path, watch: bool) -> Engine {
    let e = Engine::open(dir, opts(watch)).unwrap();
    assert!(e.wait_ready(Duration::from_secs(20)));
    e.flush();
    e
}

fn link_dump(e: &Engine) -> String {
    let s = e.snapshot();
    let mut notes: Vec<_> = s.notes().collect();
    notes.sort_by(|a, b| a.path.cmp(&b.path));
    let mut out = String::new();
    for n in notes {
        out.push_str(&format!("{}  (title={}, aliases={:?}, tags={:?})\n", n.path, n.title, n.aliases, n.tags));
        for l in &n.links {
            let dst = l.dst().and_then(|d| s.get(d)).map(|d| d.path.clone()).unwrap_or_else(|| "UNRESOLVED".into());
            let sub = match l.subpath(&s.syms) {
                Some(vault_core::Subpath::Heading(h)) => format!("#{h}"),
                Some(vault_core::Subpath::Block(b)) => format!("#^{b}"),
                None => String::new(),
            };
            out.push_str(&format!("  {}{}{} -> {}\n", if l.embed() { "!" } else { "" }, s.sym(l.target), sub, dst));
        }
        let mut back: Vec<String> = s.backlinks(n.id).iter().filter_map(|b| s.get(*b)).map(|b| b.path.clone()).collect();
        back.sort();
        out.push_str(&format!("  backlinks: {back:?}\n"));
    }
    out
}

#[test]
fn golden_links_and_backlinks() {
    let d = sample();
    let e = open(d.path(), false);
    insta::assert_snapshot!("sample_vault_links", link_dump(&e));
    let s = e.snapshot();
    assert!(s.by_path("Ignored/Secret.md").is_none(), "userIgnoreFilters respected");
    assert!(s.by_path("assets/diagram.png").is_some_and(|x| !x.markdown));
    assert!(e.unresolved().iter().any(|(t, _)| t == "missing note"));
    e.shutdown();
}

#[test]
fn search_switch_and_discovery() {
    let d = sample();
    let e = open(d.path(), false);
    let sw = e.switch("ada", 5);
    assert_eq!(sw[0].note.path, "People/Ada Lovelace.md");
    assert_eq!(sw[0].alias.as_deref(), Some("Ada"));
    assert_eq!(e.switch("gamma", 5)[0].note.path, "Gamma.md");
    let r = e.search_text("programmer", 10).unwrap();
    assert_eq!(r[0].note.path, "People/Ada Lovelace.md");
    let r = e.search("ship decided tag:project", 10).unwrap();
    assert_eq!(r[0].note.path, "Projects/Beta.md");
    let bl = e.backlinks("Projects/Alpha.md").unwrap();
    assert!(bl.iter().any(|c| c.note.path == "Home.md" && c.line.contains("[[Alpha]]")));
    // Alpha mentions "Gamma" without linking it.
    let um = e.unlinked_mentions("Gamma.md", 10).unwrap();
    assert!(um.iter().any(|c| c.note.path == "Projects/Alpha.md"), "{um:?}");
    assert!(e.wait_embeddings(Duration::from_secs(20)));
    e.recompute_graph();
    let rel = e.related("Home.md", 5).unwrap();
    assert!(!rel.is_empty());
    let tags = e.tags();
    assert!(tags.iter().any(|t| t.tag == "project" && t.count == 2), "{tags:?}");
    let g = e.graph(None, 1, 5000).unwrap();
    assert_eq!(g.nodes.len(), 6);
    assert!(!e.clusters(1).is_empty());
    let dl = e.list_dir("");
    assert!(dl.dirs.iter().any(|d| d.name == "Projects" && d.notes == 2));
    e.shutdown();
}

#[test]
fn writes_rename_delete_and_conflicts() {
    let d = sample();
    let e = open(d.path(), false);
    // create + link resolution of a previously missing target
    let out = e.create_note("", "Missing Note", "# Missing Note\n\nNow it exists.").unwrap();
    assert_eq!(out.path, "Missing Note.md");
    let s = e.snapshot();
    let home = s.by_path("Home.md").unwrap();
    let missing = s.by_path("Missing Note.md").unwrap();
    assert!(home.links.iter().any(|l| l.dst() == Some(missing.id)), "unresolved link resolves on create");
    assert!(s.backlinks(missing.id).contains(&home.id));
    // rename rewrites links
    let to = e.rename("Gamma.md", "Projects/Gamma Renamed.md").unwrap();
    assert_eq!(to, "Projects/Gamma Renamed.md");
    let home_src = std::fs::read_to_string(d.path().join("Home.md")).unwrap();
    assert!(home_src.contains("[[Gamma Renamed#Plan]]"), "{home_src}");
    let s = e.snapshot();
    let g = s.by_path("Projects/Gamma Renamed.md").unwrap();
    assert!(s.backlinks(g.id).contains(&s.by_path("Home.md").unwrap().id));
    // conflict detection
    let v = e.read_note("Projects/Beta.md").unwrap();
    std::fs::write(d.path().join("Projects/Beta.md"), "changed outside").unwrap();
    assert!(matches!(e.save_note("Projects/Beta.md", "mine", Some(&v.hash)), Err(vault_core::CoreError::Conflict { .. })));
    // delete goes to .trash and unlinks
    e.delete("Projects/Alpha.md").unwrap();
    assert!(d.path().join(".trash/Alpha.md").exists());
    let s = e.snapshot();
    let home = s.by_path("Home.md").unwrap();
    let alpha_link = home.links.iter().find(|l| &*s.sym(l.target) == "Alpha").unwrap();
    assert_eq!(alpha_link.dst().and_then(|x| s.get(x)).map(|x| x.path.as_str()), Some("Archive/Old/Alpha.md"), "falls back to the other Alpha");
    e.shutdown();
}

#[test]
fn watcher_reflects_external_edits_quickly() {
    let d = sample();
    let e = open(d.path(), true);
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    std::fs::write(d.path().join("New External.md"), "# New External\n\nLinks to [[Home]] and the word zymurgy.").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen_backlink = None;
    let mut seen_search = None;
    while Instant::now() < deadline && (seen_backlink.is_none() || seen_search.is_none()) {
        let s = e.snapshot();
        if seen_backlink.is_none()
            && let (Some(h), Some(n)) = (s.by_path("Home.md"), s.by_path("New External.md"))
            && s.backlinks(h.id).contains(&n.id)
        {
            seen_backlink = Some(t.elapsed());
        }
        if seen_search.is_none() && !e.search_text("zymurgy", 5).unwrap().is_empty() {
            seen_search = Some(t.elapsed());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (b, q) = (seen_backlink.expect("backlink seen"), seen_search.expect("search hit seen"));
    eprintln!("external edit -> backlinks {b:?}, search {q:?}");
    assert!(b < Duration::from_millis(300) && q < Duration::from_millis(600), "{b:?} {q:?}");
    // App's own write is not re-processed as external.
    let mut rx = e.subscribe();
    e.save_note("Gamma.md", "# Gamma\n\nedited", None).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let mut externals = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.external && ev.paths.iter().any(|p| p == "Gamma.md") {
            externals += 1;
        }
    }
    assert_eq!(externals, 0, "own write echoed as external");
    e.shutdown();
}

#[test]
fn snapshot_reopen_skips_reparse() {
    let d = sample();
    {
        let e = open(d.path(), false);
        e.persist().unwrap();
        e.shutdown();
    }
    let e = open(d.path(), false);
    let t = e.timings();
    assert!(t.snapshot_bytes > 0);
    assert_eq!(t.files_reparsed, 0, "warm start must not reparse unchanged files");
    assert_eq!(e.backlinks("Projects/Alpha.md").unwrap().len(), 2);
    assert_eq!(e.search_text("programmer", 5).unwrap().len(), 1);
    e.shutdown();
}

#[test]
fn move_folder_into_another_folder() {
    let d = sample();
    let e = open(d.path(), false);
    // drag "Projects" into "Archive": the folder moves with its notes
    let to = e.rename_folder("Projects", "Archive/Projects").unwrap();
    assert_eq!(to, "Archive/Projects");
    assert!(d.path().join("Archive/Projects/Beta.md").exists());
    assert!(!d.path().join("Projects").exists());
    e.flush();
    let l = e.list_dir("Archive");
    assert!(l.dirs.iter().any(|x| x.path == "Archive/Projects"), "moved folder is listed under its new parent");
    // a folder can't be dropped inside itself
    assert!(matches!(e.rename_folder("Archive", "Archive/Projects/Archive"), Err(vault_core::CoreError::Invalid(_))));
    // and won't overwrite an existing folder
    assert!(matches!(e.rename_folder("Archive/Projects", "People"), Err(vault_core::CoreError::Exists(_))));
    e.shutdown();
}
