# Vault - rules for Claude Code
- Source of truth: the Markdown files. .vault/ is a cache and must be fully rebuildable.
- Never block the UI thread: no file I/O, parsing, indexing, embedding or locks in vault-app.
- Nothing that scales with vault size runs before the first frame.
- All engine logic lives in vault-core crates; vault-app only renders snapshots and sends commands.
- Writes are atomic (temp file + rename). Never modify .obsidian/.
- Every feature ships with tests; parsing/link resolution uses golden tests on sample vaults.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` before finishing a task.
- Run the benchmark suite for any change touching vault-fs/parse/store/search/embed/graph;
  do not merge if a Section 8 budget regresses >10% without a note in docs/decisions.md.
- Verify crate versions/APIs on docs.rs before adding deps; prefer pure-Rust crates.
- Target: aarch64-apple-darwin (M5). GPU paths (Metal) must have a CPU fallback.
- Log decisions in docs/decisions.md; keep this file short and current.

## Project map
- `crates/vault-*`: engine (fs, parse, store, search, embed, graph, core), `vault-cli` (`vault`),
  `vault-server` (localhost API + embedded UI + MCP at `/mcp`, see `mcp.rs`; Ask answers in `ai.rs`,
  recall in `vault-core/src/recall.rs`), `vault-app` (Tauri shell, `-p vault-app`).
- `ui/`: Cēra-style front end (vanilla JS + CodeMirror 6). `npm run build` writes `ui/dist`,
  which vault-server embeds at compile time. Commit `ui/dist` after UI changes.
- `tools/gen-vault`: synthetic vaults. `vault bench <vault>` measures the Section 8 budgets.
- Golden test vault: `crates/vault-core/tests/fixtures/sample-vault` (insta snapshot).
