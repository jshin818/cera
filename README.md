# Cēra · Vault engine

A fast, local-first Markdown notes app for macOS: the look and feel of **Cēra**, built on the
**Vault** engine (Rust). Your notes stay plain `.md` files in a folder — any Obsidian vault works —
and everything else (search index, embeddings, graph) is a rebuildable cache in `.vault/`.

- Opens the last note straight from disk before any indexing (first frame ~10–20 ms at 100k notes).
- ⌘K finds notes by title, alias or path (typo-tolerant) and searches text by meaning + keywords.
- Backlinks, unlinked mentions with one-click linking, related notes, link and tag suggestions,
  near-duplicate warnings, Leiden topic clusters, a graph view, vault stats.
- Obsidian-compatible: `[[links]]`, `[[note#heading]]`, `[[note#^block]]`, `![[embeds]]`, aliases,
  nested `#tags`, frontmatter, `.obsidian/app.json` ignore filters and attachment folder.
- No cloud. Embeddings run locally (Model2Vec static models, or a built-in hashed fallback).

## Run it on the Mac (Apple Silicon)

```bash
xcode-select --install
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```bash
cargo build --release -p vault-cli
```

```bash
./target/release/vault serve ~/Notes --open
```

That serves the app in your browser. For the desktop app (Tauri 2, native WKWebView window):

```bash
cargo install tauri-cli --version "^2" --locked
```

```bash
cd crates/vault-app && cargo tauri build
```

The app bundle lands in `target/release/bundle/macos/Cera.app`. It asks for a vault folder on
first launch and remembers it. To switch vaults, click the vault's name at the bottom of the
sidebar: it lists recent vaults and can open any folder as a vault. Unsigned builds run locally; distribution needs Apple signing and
notarization.

Optional, better semantic search: put a Model2Vec model (for example `minishlab/potion-base-8M`:
`model.safetensors` + `tokenizer.json`) in `~/.vault-models/potion-base-8M/`. It's picked up on the
next launch and the vault is re-embedded in the background.

## CLI

```bash
vault index ~/Notes --full         # index + embeddings + graph metrics, print stats
vault search ~/Notes '"zero copy" tag:rust -draft'
vault switch ~/Notes kalo           # quick switcher
vault backlinks ~/Notes "Projects/Plan.md"
vault related ~/Notes "Projects/Plan.md"
vault mentions ~/Notes "Projects/Plan.md"
vault clusters ~/Notes
vault dupes ~/Notes
vault bench ~/vault-100k            # Section 8 budgets; --baseline old.json gates >10% regressions
```

Synthetic vaults for benchmarking: `cargo run --release -p gen-vault -- ~/vault-100k --notes 100000`.

## Ask your notes

⌘J (or a question ending in "?" on Home) opens **Ask**. Cēra finds the passages that answer it,
numbered, with the note, section, line, edit date and why each was picked, how those notes link to
each other, and what the notes leave out. An answer model, if you set one up, writes a short answer
with each citation as a pill in the text; click a pill to read that passage, or "sources" under the
answer for the list of all of them, and open a source to land on the exact lines. "Save" keeps the
answer as a note in Inbox linked to its sources.

Settings → AI answers picks the model: **Claude** (your Anthropic API key, or `ANTHROPIC_API_KEY`),
a **local** OpenAI-compatible server such as Ollama (`localhost:11434`) or LM Studio
(`localhost:1234`), or **off** for sources only. The key is kept in
`~/Library/Application Support/Cera/ai.json`, never in the vault. Only the question and the
matching passages are sent, and only to the model you chose.

## AI agents (MCP)

Cēra speaks the Model Context Protocol, so agents such as Claude Code, Claude Desktop, Cursor or
Codex can search and read your notes. Add this to the agent's MCP settings:

```json
{ "mcpServers": { "cera": { "command": "/path/to/vault", "args": ["mcp"] } } }
```

While the Cēra app is open, `vault mcp` goes through it; otherwise pass the vault folder
(`"args": ["mcp", "/Users/you/Notes"]`). Add `"--write"` to let the agent create notes (in
`Inbox/`) and append to notes. It never deletes or renames, and every write is listed in
`.vault/agent-log.jsonl`. Tools: `context` (the cited passages that answer a question), `search`,
`read_note`, `list_folder`, `recent`, `backlinks`, `related`, `tags`, plus `create_note` and
`append_to_note` with `--write`. Notes are also readable as `cera://note/<path>` resources.

## Keyboard

| Keys | Action |
| --- | --- |
| ⌘K / ⌘O | Find or create a note, search text |
| ⌘P | Commands |
| ⌘⇧F | Search view (meaning + words, or exact words) |
| ⌘N | New note |
| ⌘J | Ask your notes |
| ⌘G | Graph for the current note |
| ⌘-click | Open a link or result in a new tab; ⌘W closes the tab |
| ⌘B ⌘I ⌘E ⌘⇧H | Bold, italic, code, highlight |
| ⌘↵ | Toggle task |
| `[[` `/` `#` | Link completion, block menu, tag completion |
| ⌘\\ | Toggle sidebar |

## Layout

```
crates/vault-fs       walker, FSEvents/inotify watcher, XXH3, atomic writes, .obsidian config
crates/vault-parse    pulldown-cmark extraction, Obsidian link resolution, chunker
crates/vault-store    redb tables, rkyv mmap startup snapshot (file table + links)
crates/vault-search   tantivy BM25F, nucleo switcher (+frizbee typos), frecency, RRF
crates/vault-embed    Model2Vec loader, hashed fallback, binary + int8 (mmap) vector index
crates/vault-graph    CSR graph, PageRank, PPR, Leiden, Adamic-Adar, Aho-Corasick mentions, MinHash, layout
crates/vault-core     the engine: snapshots, indexer, write path, queries
crates/vault-server   localhost JSON API + SSE + embedded UI + MCP endpoint for agents
crates/vault-cli      `vault` binary
crates/vault-app      Tauri 2 desktop shell
ui/                   Cēra front end (vanilla JS, CodeMirror 6), built into ui/dist
tools/gen-vault       synthetic vault generator
benches/              divan micro-benchmarks and a parse profiler
docs/decisions.md     every deviation from the spec and why
```

## Performance (Section 8)

Measured with `vault bench` on a synthetic **100,000-note vault** (524 MB of Markdown, 4.5M links,
10k attachments), on the **4-vCPU Linux build container** — not the M5 MacBook Pro the budgets
target. p95 unless noted.

| Interaction | Budget | Measured | |
| --- | --- | --- | --- |
| Cold start → last note in memory | < 150 ms | 18.5 ms | ✅ |
| Warm start | < 50 ms | 12.0 ms | ✅ |
| Snapshot load (mmap + verify) | < 15 ms | 3.5 ms | ✅ |
| Quick switcher per keystroke | < 5 ms | 3.7 ms p50 · 8.3 ms p95 | ⚠️ |
| Full-text search | < 30 ms | 13.7 ms | ✅ |
| Hybrid search (BM25 + vector + RRF) | < 50 ms | 22.0 ms | ✅ |
| Related notes panel | < 30 ms | 8.5 ms uncached · 0.01 ms cached | ✅ |
| Open / switch note (engine side) | < 16 ms | 0.04 ms | ✅ |
| External edit → visible in search/backlinks | < 300 ms | 204 ms | ✅ |
| First index of new 100k vault (text) | < 10 s | 17.9 s | ⚠️ |
| First embedding pass | < 30 s | 12.0 s (hashed embedder) | ✅ |
| Idle memory | < 300 MB | 291 MB anonymous (304 MB RSS) | ✅ |

⚠️ rows are CPU-bound and fail on this 4-vCPU container; they need re-measuring on the M5 (more and
faster cores, background work on efficiency cores). Not measured here: keystroke → rendered frame,
the GPU graph at 60 FPS, and Vault.app cold start including window paint.
