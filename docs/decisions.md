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
  only. Cēra's Return page, Share and dictation are not carried over (cloud services are a non-goal;
  WKWebView has no Web Speech recognition). Chat came back as **Ask** (Nov 2026, see below), with
  the model opt-in and local-first.
- **Delete** moves files to `<vault>/.trash/` (Obsidian's local-trash convention), never unlinks.
- **Tabs** appear only when more than one note is open (⌘-click a link or result).
- **Sidebar is one tree** (owner request, Nov 2026). Cēra's separate "Folders" and "Notes" groups are
  gone: every level, the top level included, lists its folders and then its notes, like Finder and
  Obsidian. New folder moved to the sidebar header.
- **Drag and drop everywhere in the tree.** Notes and folders drop onto a folder, into an open
  folder's contents, or onto empty space (top level); hovering a closed folder springs it open.
  Folder moves use `rename_folder`, which now refuses moving a folder inside itself.
  Attachments are draggable too (`rename` already rewrites links to them).
- **Desktop shell disables Tauri's drag-drop handler** (`disable_drag_drop_handler`). With it on,
  wry answers every macOS drag itself (NSDragOperationCopy, the green "+") and never forwards it to
  WebKit, so HTML5 drag and drop in the sidebar did nothing in the app while working in browsers.
  Cēra doesn't use OS file drops through Tauri, so nothing is lost.
- **"Current folder" decides where new things go** (owner request, Nov 2026). Opening a folder or
  clicking a note inside one makes that folder current; clicking a top-level note or empty space in
  the tree makes the top level current; closing a folder hands it to its parent. It only counts
  while open (else the nearest open ancestor). New note (button, ⌘N, home box, ⌘K create) and New
  folder use it; following a link to a missing note still creates it next to the linking note
  (Obsidian behaviour). The current folder's icon is honey. Stored in `localStorage`
  (`cera.activeFolder`).
- **New folder is Finder-style:** it is created at once as "New folder" (or "New folder 2", …)
  inside the current folder and opens in rename mode; Enter/blur keeps the typed name, Escape keeps
  "New folder". Previously an empty name box appeared at the top of the tree and clicking away
  silently cancelled. Sidebar re-renders are deferred while a name box is open so live updates
  can't wipe it, and `renderFolder` now resolves only after open subfolders are drawn.
- **Properties sit on the tag row** (owner request, Nov 2026). Frontmatter is hidden in the editor
  until edited; a "Properties · keys" chip at the start of the tag row (tags left out of the summary,
  since their chips are beside it) puts the cursor in it. Notes without frontmatter show no chip.
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

## Switching vaults

- **In place, one vault per window.** The app opens the new vault's engine first, starts a second
  localhost server for it, points the window at it and then stops the old server (which finishes
  in-flight requests, ends its event streams and closes its engine). A vault that can't be opened
  (gone, or held by another process through redb's lock) is reported and the current vault stays.
  Opening several vaults side by side, as Obsidian does with windows, is not built.
- **The app owns the list.** Recent vaults live in the app's `config.json` (`recent`, newest first,
  12 kept) next to `vault`. vault-server only sees a `VaultHost` trait (recent, forget, folder
  picker, switch), so `vault serve` has no switcher and the button just names the vault. The
  native folder panel runs on the main thread via `run_on_main_thread`.
- **Agents follow the app.** Each server republishes `server.json`, and `vault mcp` without a vault
  argument looks the app up again when a forward fails, so an agent keeps working after a switch
  and sees the vault the app shows. With a vault argument it only reconnects to that vault.

## AI agents (MCP)

- **One handler, two transports.** `vault_server::mcp::Mcp::handle` answers JSON-RPC; the running
  server exposes it at `POST /mcp` (Streamable HTTP, single JSON responses, no SSE) and `vault mcp`
  speaks it over stdio. Agents need no plugin or copied key.
- **Hand-written instead of the `rmcp` SDK.** The surface is small (tools, resources, discover), it
  adds no dependencies, and one handler serves both the stateless 2026-07-28 revision
  (`server/discover`, `_meta` protocol version, `resultType`, `ttlMs`/`cacheScope`) and the 2025
  revisions with `initialize`, which most clients still send. rmcp 3.9.0 was a day old when checked.
  Not built, as 2026-07-28 deprecates them: roots, sampling, logging. Not built yet: subscriptions.
- **`vault mcp` forwards to the running app.** redb takes an exclusive file lock, so a second process
  can't open a vault the app has open. The app writes `{url, token, vault, pid}` (mode 0600) to
  `~/Library/Application Support/Cera/server.json` when its server is listening and removes it on a
  clean exit; `vault mcp` pings it and forwards when the vault matches, else opens the vault itself.
  `CERA_DISCOVERY` overrides the file's location. A stale file just fails the ping.
- **Auth:** `/mcp` sits behind the same token as `/api` and also accepts `Authorization: Bearer`.
  Requests with a non-localhost `Origin` are refused (DNS rebinding).
- **Agent-shaped tools.** search, read_note (one heading at a time; 20k-character cap), list_folder,
  recent, backlinks, related, tags. Notes can be named by path, title or alias. Results carry paths,
  sections and plain-text excerpts (snippet HTML is stripped).
- **Writes are opt-in and additive.** `--write` (or `x-vault-mcp-write: 1` over HTTP) adds
  create_note (default folder `Inbox/`) and append_to_note (to the end, or to a section; a missing
  heading is created; a list item continues the list). No delete, rename or overwrite. Appends
  pass the read hash, so a note edited meanwhile is refused instead of clobbered. Each write is
  appended to `.vault/agent-log.jsonl`; that log is a convenience record, so losing it with the
  cache loses only history.

## Ask and recall (Nov 2026)

Owner request: a single-user, local AI memory hub; a chat that recalls from the notes and shows
results with sources ("the sources part is really important"). Answer models chosen by the owner:
a local model or Claude, picked in Settings; with neither, sources only.

- **Recall is engine work** (`vault-core/src/recall.rs`), shared by the Ask view, `/api/recall` and
  the MCP `context` tool. Candidates: BM25 notes + notes of the best vector chunks (RRF), plus one
  hop of links from the top three (answers often sit in a linked note). Every chunk of a candidate is
  scored by cosine with the question and by words (BM25-style saturation, IDF over the candidate
  chunks, light stemming so "shipping" meets "ship"); the two are fused as normalised scores with the
  note's rank, nudged by PageRank and frecency. Rank fusion was tried first and flattened a clear
  winner into a tie that note popularity decided. At most two passages per note, near-identical
  excerpts dropped, ~12k characters total.
- **A citation points at lines, not sections.** The excerpt is the best paragraph run inside the
  chunk (≤ ~900 bytes), returned with its byte range, first line, heading path, edit date and the
  reasons it was picked ("matches pricing", "similar meaning", "linked from Home"). The UI opens the
  note with that exact range selected.
- **Links and gaps are part of the answer.** Recall reports which source notes link to each other,
  and what the notes don't settle: no match, only a loose match, a linked note that was never written
  (an unresolved `[[link]]` whose name touches the question), a source not edited for a year, and
  sources that are near-duplicates (MinHash) and may disagree.
- **The model is opt-in, and this reverses "no cloud" only by choice.** `provider`: auto (Claude when
  a key is set, else a local server if one answers), claude, local, off. Claude goes through the
  Messages API over raw HTTP (no Rust SDK): `claude-opus-5-5` by default, streaming, `effort: low`,
  server-side fallback on for Opus/Sonnet 5.5. Local is any OpenAI-compatible server on loopback
  (Ollama, LM Studio); other hosts are refused. The prompt carries only the numbered passages and
  asks for `[n]` citations and for gaps to be said, not filled.
- **The key never lives in the vault.** `<config>/Cera/ai.json` (mode 0600, atomic write), or
  `ANTHROPIC_API_KEY`. The API reports `has_key`/`key_source`, never the key. `CERA_CONFIG_DIR`
  overrides the folder.
- **Ask reads as one column of text.** The owner asked for a chat-like page with no side panel and
  minimal chrome. Citations are pills inside the answer, named by the note (a leading date is
  dropped; runs like `[2][3]` become one pill, "Pricing review +1"). A pill opens that passage in a
  modal; "n sources" under the answer opens the list of all of them. Gaps about the question stay in
  the thread as one faint line; gaps about one source (stale, near-duplicate) live in its modal.
  With no model, the reply is the top passages quoted, each ending in its pill.
- **Saved answers are notes.** "Save to Inbox" writes the answer with its sources as wikilinks to the
  cited headings, so answers join the graph and future recalls. (They can be recalled as sources
  too; if that gets circular, rank `Inbox/` answers below primary notes.)
- **reqwest 0.13.5 with rustls** (its default TLS) for the model calls. It pulls aws-lc-rs, which
  builds C; ring via `rustls-no-provider` was the pure-ish alternative but needs a provider install
  at startup. Revisit if build time matters.
- **Bench:** new row "Recall (cited passages for Ask)", 100 ms budget (no spec row). 10k-note vault on
  the M-series Mac: p50 2.5 ms, p95 3.2 ms with the hashed embedder. Existing rows unchanged.

## Not done (later phases)

- Agents: change subscriptions, edit_section and set_properties tools, a write toggle and "what
  the agent changed" view with undo in Settings, an `.mcpb` bundle, `AGENTS.md` in the vault.
- Phase 3: EmbeddingGemma upgrade, ColBERT/cross-encoder reranker, YAKE tag keyphrases.
- Ask: contextual chunk headers (title + heading path embedded with each chunk), a reranker over the
  recall candidates, typed links (`decided-by::`, `owner::`) and entity notes for people/projects,
  answer-quality evals on a golden question set, tool use (let the model call recall again).
- Phase 4: WASM plugin host, Dataview-like property queries (frontmatter is already in redb `notes`).
- Phase 5: Loro sync, BLAKE3 blob store, E2EE relay, iOS shell.
- Absolute budgets were measured on a 4-vCPU Linux container, not the M5; see README.
