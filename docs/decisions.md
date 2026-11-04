# Decisions

Spec: "Vault: Build Specification" (Nov 2026). Product brief from the owner: look and feel of
**Cēra** (the earlier web notes app), technology of **Vault**. Entries are newest last.

## Product and shell

- **Brand and UI.** The app is presented as Cēra (wordmark `cēra`, honeycomb logo, Inter, honey
  accent, Cēra's tokens, sidebar, home composer, ⌘K box, tag chips). Engine crates keep the spec's
  `vault-*` names; "Vault" was a placeholder product name.
- **Shell: Tauri 2 + localhost server, not GPUI.** The owner wants Cēra's exact look, which is an
  HTML/CSS design; GPUI would mean re-implementing it in a different widget model, could not be
  verified in this Linux build environment, and is pre-1.0. The spec names Tauri 2 + CodeMirror 6 as
  the sanctioned fallback (Sections 3 and 10) with vault-core unchanged. The UI talks to
  `vault-server` (axum on 127.0.0.1, random port, per-launch token cookie) so the same code runs in
  the desktop window and in a browser (`vault serve`). The engine never runs on the WebView's thread.
- **Editor: CodeMirror 6 with live preview** (Obsidian-style, as the risk table recommends) instead
  of Cēra's contenteditable HTML. Markdown stays byte-for-byte the source of truth. Syntax hides on
  lines the cursor is not on; wikilinks render honey-underlined, `#tags` as Cēra chips, tasks as
  checkboxes, embeds inline, frontmatter as a "Properties" pill. `[[` completes notes, `/` opens blocks.
- **Cēra rules kept:** folders nest and start closed, one top-level folder open at a time, plain
  folder icons, tags as coloured chips, light motion (opacity/transform). **Changed:** the note title
  is the file name (Obsidian semantics), so renaming the title renames the file and rewrites links.
  **Graph view** was removed from Cēra but is a Vault Phase 3 deliverable; it lives under Explore
  only. Cēra's Return page, Share, Chat (needs a cloud LLM) and dictation are not carried over
  (cloud services are a non-goal; WKWebView has no Web Speech recognition).
- **Delete** moves files to `<vault>/.trash/` (Obsidian's local-trash convention), never unlinks.
- **Tabs** appear only when more than one note is open (⌘-click a link or result).
- **Sidebar is one tree** (owner request, Nov 2026). Cēra's separate "Folders" and "Notes" groups are
  gone: every level, the top level included, lists its folders and then its notes, like Finder and
  Obsidian. New folder moved to the sidebar header.
- **Drag and drop everywhere in the tree.** Notes and folders drop onto a folder, into an open
  folder's contents, or onto empty space (top level); hovering a closed folder springs it open.
  Folder moves use `rename_folder`, which now refuses moving a folder inside itself.
- **Graph scopes are named "This note" / "All notes"** instead of Local / Global; switching reloads
  only the canvas and the toolbar keeps its size.
- **Slash menu** shows an icon tile and a plain label per block (no raw Markdown syntax) and forces
  the app font over CodeMirror's monospace tooltip default.
- **Settings** sheet from the sidebar's bottom-left (⌘,): theme, text size, line width, spell check,
  folder note counts, vault rescan, shortcuts. UI-only preferences in `localStorage`
  (`cera.settings`), applied before first paint; nothing is written to the vault or `.obsidian/`.

## Engine

- **Threads:** std threads + crossbeam instead of a tokio runtime inside vault-core (FSEvents/inotify
  already has its own thread and all work is blocking I/O or CPU). Tokio is used only by the server.
  Threads: indexer (watch → debounce → parse → apply → commit), embed worker, background
  (switcher/graph/persist), redb writer. Background threads run at utility QoS on macOS (E-cores)
  and nice 10 on Linux.
- **Snapshots:** `Arc<Snapshot>` via arc-swap. Entries are `Arc<Entry>`; backlinks live in a separate
  copy-on-write table so a new link never clones the target note's entry.
- **Links are interned** (`OutLink` is 20 bytes: target symbol, dst, start, len, flags, one optional
  symbol for subpath+display). At 100k notes / 4.5M links this took idle memory from 1.66 GB to
  ~290 MB, together with the items below.
- **Startup snapshot is split:** `snapshot.rkyv` (file table, ~19 MB at 100k) is checksum-verified on
  the critical path and hydrated first, so the sidebar and switcher work in ~240 ms (4 vCPU);
  `links.rkyv` is verified and attached after (~550 ms). xxh3 checksum replaces bytecheck validation.
- **redb is a mirror, written off-thread** with a 16 MiB page cache and opened lazily, so it is never
  on the startup path and an unclean exit can't put redb recovery in front of the first frame.
- **First index:** paths are pre-registered with the resolver so every link resolves once; batches of
  4,000 are parsed while the previous batch is applied; intermediate snapshots omit backlinks.
  Embedding starts only after the text index is ready (it never competes with it).
- **Commit policy:** an isolated change commits after a 30 ms quiet window; bursts commit at most
  every 250 ms. External edit → searchable p95 ≈ 200 ms with the 100 ms debounce.
- **Memory release:** after hydrate, first index, graph metrics and persistence the engine calls
  `malloc_trim` (glibc) / `malloc_zone_pressure_relief` (macOS); glibc's mmap threshold is pinned at
  256 KB. mimalloc and jemalloc were measured and were worse for this workload.
- **Re-resolution on rename/create** uses the unresolved map (keyed by link name) plus the backlinks
  of same-named files; aliases only re-resolve links using that alias name.

## Search and discovery

- **Tantivy** schema per §6.1; `title_prefix` is indexed as explicit word prefixes (1–15 chars).
  Fuzzy distance 0/1/2 by term length; AND semantics with OR fallback; filter-only queries
  (`tag:x`) list matches.
- **Switcher:** nucleo over titles/aliases/paths with a character-bitmask prefilter and incremental
  narrowing (typing a character rescans only previous matches); frecency boosts update in place.
- **Embeddings:** the Model2Vec loader is our own (safetensors + WordPiece/Unigram vocab from
  tokenizer.json), so no network or Python is needed. Put a model in `~/.vault-models/potion-base-8M/`
  (or set `VAULT_MODELS`). Without one, a hashed-feature embedder (words, bigrams, char trigrams) is
  used: always available, lower quality. **EmbeddingGemma (Phase 3) is not wired**: it needs ONNX
  Runtime/Metal binaries and a model download this environment could not fetch or test.
- **Vector index:** binary codes in RAM, int8 rescoring vectors memory-mapped (`vectors/index.q8`).
- **Unlinked mentions** of a note come from a phrase query for its title/aliases verified with
  word-boundary checks (always current); the Aho-Corasick automaton over all titles powers
  "mentioned here" link suggestions inside a note and is rebuilt lazily when titles change.
- **Graph:** own CSR instead of petgraph (memory/speed at 4.5M edges). Leiden with refinement plus a
  final connected-components split guarantees connected communities. **Graph view uses a CPU
  Barnes-Hut layout + Canvas 2D**, not wgpu compute: the WebView shell can't host a wgpu surface,
  and the layout persists to `layout.bin` and is reused (no explosion). Above 20k nodes the global
  graph draws Leiden clusters as aggregate nodes (level of detail).
- **Near-duplicates:** one-permutation MinHash (64 bins, densified), 16×4 LSH bands, intrusive
  linked-list buckets (~16 B per note-band).

## Not done (later phases)

- Phase 3: EmbeddingGemma upgrade, ColBERT/cross-encoder reranker, YAKE tag keyphrases.
- Phase 4: WASM plugin host, Dataview-like property queries (frontmatter is already in redb `notes`).
- Phase 5: Loro sync, BLAKE3 blob store, E2EE relay, iOS shell.
- Absolute budgets were measured on a 4-vCPU Linux container, not the M5; see README.
