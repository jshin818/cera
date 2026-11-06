//! Model Context Protocol endpoint: lets AI agents search, read and (when allowed) add to the vault.
//!
//! JSON-RPC over two transports: `POST /mcp` on the running server (Streamable HTTP, JSON
//! responses only) and stdio through `vault mcp`. Both call [`Mcp::handle`]. Written by hand
//! rather than with an SDK: the protocol surface is small, and this way one handler speaks both the
//! stateless 2026-07-28 revision (`server/discover`, per-request `_meta`) and the 2025 revisions
//! with an `initialize` handshake, which most clients still send.
//!
//! Agents get paths, headings and short excerpts first and whole notes only on request. Writes are
//! opt-in (`write`), never delete or rename, go to `Inbox/` by default, refuse to overwrite a note
//! that changed underneath them, and are appended to `.vault/agent-log.jsonl`.

use serde_json::{Value, json};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use vault_core::{CoreError, Engine};

pub const LATEST: &str = "2026-07-28";
pub const SUPPORTED: &[&str] = &["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"];
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const DEFAULT_FOLDER: &str = "Inbox";
const READ_LIMIT: usize = 20_000;

const INSTRUCTIONS: &str = "Cēra is the user's local Markdown notes vault. Notes are plain .md files; paths are relative to \
the vault. Use `search` first (it matches meaning and words), then `read_note` for the notes you need; pass `heading` to read \
one section. `related` and `backlinks` follow the user's own links. Notes accept [[wikilinks]] and #tags. When writing, keep \
the user's voice, prefer appending to existing notes over creating near-duplicates, and put new notes in Inbox unless asked.";

#[derive(Clone)]
pub struct Mcp {
    pub engine: Engine,
    /// Expose `create_note` and `append_to_note`.
    pub write: bool,
}

/// A JSON-RPC error: (code, message).
type RpcError = (i64, String);

impl Mcp {
    pub fn new(engine: Engine, write: bool) -> Self {
        Self { engine, write }
    }

    /// Handle one JSON-RPC message. Returns `None` for notifications (no reply is sent).
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if id.is_none() || method.starts_with("notifications/") {
            return None;
        }
        let res = if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_empty() {
            Err((-32600, "invalid request".into()))
        } else {
            self.dispatch(method, &params)
        };
        Some(match res {
            Ok(mut result) => {
                if let Some(o) = result.as_object_mut() {
                    o.insert("resultType".into(), json!("complete"));
                    o.entry("_meta").or_insert_with(|| json!({})).as_object_mut().map(|m| m.insert("io.modelcontextprotocol/serverInfo".into(), server_info()));
                }
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        })
    }

    fn dispatch(&self, method: &str, p: &Value) -> Result<Value, RpcError> {
        if let Some(v) = p.pointer("/_meta").and_then(|m| m.get(META_VERSION)).and_then(Value::as_str)
            && !SUPPORTED.contains(&v)
        {
            return Err((-32022, format!("unsupported protocol version {v}; supported: {}", SUPPORTED.join(", "))));
        }
        match method {
            "initialize" => {
                let asked = p.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version = if SUPPORTED.contains(&asked) { asked } else { "2025-11-25" };
                Ok(json!({"protocolVersion": version, "capabilities": capabilities(), "serverInfo": server_info(), "instructions": INSTRUCTIONS}))
            }
            "server/discover" => {
                Ok(json!({"supportedVersions": SUPPORTED, "capabilities": capabilities(), "serverInfo": server_info(), "instructions": INSTRUCTIONS}))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tools(), "ttlMs": 3_600_000, "cacheScope": "private"})),
            "tools/call" => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("");
                let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if !self.tools().iter().any(|t| t["name"] == name) {
                    return Err((-32602, format!("unknown tool {name}")));
                }
                Ok(match self.call(name, &args) {
                    Ok((text, structured)) => {
                        let mut r = json!({"content": [{"type": "text", "text": text}], "isError": false});
                        if let Some(s) = structured {
                            r["structuredContent"] = s;
                        }
                        r
                    }
                    Err(msg) => json!({"content": [{"type": "text", "text": msg}], "isError": true}),
                })
            }
            "resources/list" => {
                let notes = self.engine.recent(50);
                let resources: Vec<Value> = notes
                    .iter()
                    .filter(|n| n.markdown)
                    .map(|n| json!({"uri": note_uri(&n.path), "name": n.path, "title": n.title, "mimeType": "text/markdown"}))
                    .collect();
                Ok(json!({"resources": resources, "ttlMs": 5_000, "cacheScope": "private"}))
            }
            "resources/templates/list" => Ok(json!({
                "resourceTemplates": [{"uriTemplate": "cera://note/{+path}", "name": "note", "title": "A note by its path in the vault", "mimeType": "text/markdown"}],
                "ttlMs": 3_600_000, "cacheScope": "private",
            })),
            "resources/read" => {
                let uri = p.get("uri").and_then(Value::as_str).unwrap_or("");
                let path = uri.strip_prefix("cera://note/").map(percent_decode).ok_or((-32602, format!("unknown resource {uri}")))?;
                let n = self.engine.read_note(&path).map_err(|_| (-32602, format!("resource not found: {uri}")))?;
                Ok(json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": n.content}], "ttlMs": 0, "cacheScope": "private"}))
            }
            "prompts/list" => Ok(json!({"prompts": [], "ttlMs": 3_600_000, "cacheScope": "private"})),
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    fn tools(&self) -> Vec<Value> {
        let read = json!({"readOnlyHint": true, "openWorldHint": false});
        let note = json!({"type": "string", "description": "Note path relative to the vault (e.g. \"Projects/Plan.md\"), or its title or alias"});
        let mut t = vec![
            json!({"name": "search", "title": "Search notes",
                "description": "Find notes by meaning and keywords. Returns paths, titles, the best-matching section and an excerpt. Supports \"exact phrase\", tag:name and -word.",
                "inputSchema": {"type": "object", "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10},
                    "mode": {"type": "string", "enum": ["hybrid", "text"], "default": "hybrid", "description": "text = keywords only"}},
                    "required": ["query"]},
                "annotations": read}),
            json!({"name": "read_note", "title": "Read a note",
                "description": "Read a note's Markdown, tags, aliases and link counts. Pass heading to read only that section.",
                "inputSchema": {"type": "object", "properties": {"note": note, "heading": {"type": "string"}}, "required": ["note"]},
                "annotations": read}),
            json!({"name": "list_folder", "title": "List a folder",
                "description": "List subfolders and notes in a folder. Empty path = top level.",
                "inputSchema": {"type": "object", "properties": {"path": {"type": "string", "default": ""}}},
                "annotations": read}),
            json!({"name": "recent", "title": "Recently changed notes",
                "description": "Notes changed most recently, newest first.",
                "inputSchema": {"type": "object", "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}}},
                "annotations": read}),
            json!({"name": "backlinks", "title": "Backlinks",
                "description": "Notes that link to this note, with the line that links.",
                "inputSchema": {"type": "object", "properties": {"note": note}, "required": ["note"]},
                "annotations": read}),
            json!({"name": "related", "title": "Related notes",
                "description": "Notes similar to this one or linked near it, with the reason.",
                "inputSchema": {"type": "object", "properties": {"note": note, "limit": {"type": "integer", "minimum": 1, "maximum": 30, "default": 8}}, "required": ["note"]},
                "annotations": read}),
            json!({"name": "tags", "title": "Tags",
                "description": "All tags with counts, or the notes with one tag.",
                "inputSchema": {"type": "object", "properties": {"tag": {"type": "string", "description": "Omit to list all tags"}}},
                "annotations": read}),
        ];
        if self.write {
            let write = json!({"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false});
            t.push(json!({"name": "create_note", "title": "Create a note",
                "description": format!("Create a new note. Goes in {DEFAULT_FOLDER}/ unless folder is given; a name that is taken gets a number. Search first to avoid duplicates."),
                "inputSchema": {"type": "object", "properties": {
                    "title": {"type": "string", "description": "Becomes the file name"},
                    "content": {"type": "string", "description": "Markdown body"},
                    "folder": {"type": "string", "description": format!("Folder path, default {DEFAULT_FOLDER}; \"\" for the top level")},
                    "tags": {"type": "array", "items": {"type": "string"}}},
                    "required": ["title", "content"]},
                "annotations": write}));
            t.push(json!({"name": "append_to_note", "title": "Append to a note",
                "description": "Add Markdown to the end of a note, or to the end of one section. A missing heading is created at the end.",
                "inputSchema": {"type": "object", "properties": {"note": note, "text": {"type": "string"}, "heading": {"type": "string"}}, "required": ["note", "text"]},
                "annotations": write}));
        }
        t
    }

    /// Run a tool. Ok = (text for the model, structured result); Err = message for an isError result.
    fn call(&self, name: &str, a: &Value) -> Result<(String, Option<Value>), String> {
        let e = &self.engine;
        let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let n = |k: &str, d: usize, max: usize| a.get(k).and_then(Value::as_u64).map(|v| (v as usize).clamp(1, max)).unwrap_or(d);
        let structured = |v: Value| Ok((v.to_string(), Some(v)));
        match name {
            "search" => {
                let q = s("query");
                if q.is_empty() {
                    return Err("query is empty".into());
                }
                let limit = n("limit", 10, 50);
                let hits = if s("mode") == "text" { e.search_text(&q, limit) } else { e.search(&q, limit) }.map_err(msg)?;
                let results: Vec<Value> = hits
                    .into_iter()
                    .map(|h| json!({"path": h.note.path, "title": h.note.title, "section": h.section, "excerpt": plain(&h.snippet), "tags": h.tags, "score": h.score}))
                    .collect();
                structured(json!({"results": results}))
            }
            "read_note" => {
                let path = self.find(&s("note"))?;
                let v = e.read_note(&path).map_err(msg)?;
                let heading = s("heading");
                let body = if heading.is_empty() {
                    v.content.clone()
                } else {
                    let (from, to) = section(&v.content, &heading).ok_or_else(|| format!("no heading \"{heading}\" in {path}"))?;
                    v.content[from..to].to_string()
                };
                let (body, truncated) = cut(&body, READ_LIMIT);
                let head = json!({"path": v.path, "title": v.title, "tags": v.tags, "aliases": v.aliases, "words": v.words, "backlinks": v.backlinks, "modified_ms": v.mtime_ms, "truncated": truncated});
                Ok((format!("{head}\n\n{body}"), Some(json!({"note": head, "content": body}))))
            }
            "list_folder" => {
                let d = e.list_dir(s("path").trim_matches('/'));
                structured(json!({
                    "path": d.path,
                    "folders": d.dirs.iter().map(|f| serde_json::to_value(f).unwrap_or(Value::Null)).collect::<Vec<_>>(),
                    "notes": d.notes.iter().map(|n| json!({"path": n.path, "title": n.title, "modified_ms": n.mtime_ms})).collect::<Vec<_>>(),
                }))
            }
            "recent" => structured(
                json!({"notes": e.recent(n("limit", 20, 100)).into_iter().filter(|n| n.markdown).map(|n| json!({"path": n.path, "title": n.title, "modified_ms": n.mtime_ms})).collect::<Vec<_>>()}),
            ),
            "backlinks" => {
                let path = self.find(&s("note"))?;
                let b = e.backlinks(&path).map_err(msg)?;
                structured(
                    json!({"note": path, "backlinks": b.into_iter().map(|c| json!({"path": c.note.path, "title": c.note.title, "line": c.line})).collect::<Vec<_>>()}),
                )
            }
            "related" => {
                let path = self.find(&s("note"))?;
                let r = e.related(&path, n("limit", 8, 30)).map_err(msg)?;
                structured(
                    json!({"note": path, "related": r.into_iter().map(|r| json!({"path": r.note.path, "title": r.note.title, "why": r.why, "score": r.score})).collect::<Vec<_>>()}),
                )
            }
            "tags" => {
                let tag = s("tag");
                let tag = tag.trim_start_matches('#');
                if tag.is_empty() {
                    structured(json!({"tags": e.tags()}))
                } else {
                    structured(
                        json!({"tag": tag, "notes": e.notes_with_tag(tag).into_iter().map(|n| json!({"path": n.path, "title": n.title})).collect::<Vec<_>>()}),
                    )
                }
            }
            "create_note" if self.write => {
                let title = s("title");
                if title.is_empty() {
                    return Err("title is empty".into());
                }
                let folder = a.get("folder").and_then(Value::as_str).map(|f| f.trim().trim_matches('/').to_string()).unwrap_or_else(|| DEFAULT_FOLDER.into());
                let tags: Vec<String> = a
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|t| t.iter().filter_map(Value::as_str).map(|t| t.trim().trim_start_matches('#').replace(' ', "-")).filter(|t| !t.is_empty()).collect())
                    .unwrap_or_default();
                let body = a.get("content").and_then(Value::as_str).unwrap_or("");
                let content = if tags.is_empty() { body.to_string() } else { format!("---\ntags: [{}]\n---\n{body}", tags.join(", ")) };
                let out = e.create_note(&folder, &title, &content).map_err(msg)?;
                self.log("create_note", &out.path);
                structured(json!({"path": out.path, "created": true}))
            }
            "append_to_note" if self.write => {
                let path = self.find(&s("note"))?;
                let text = a.get("text").and_then(Value::as_str).unwrap_or("").trim_matches('\n');
                if text.trim().is_empty() {
                    return Err("text is empty".into());
                }
                let v = e.read_note(&path).map_err(msg)?;
                let content = append(&v.content, text, &s("heading"));
                e.save_note(&path, &content, Some(&v.hash)).map_err(msg)?;
                self.log("append_to_note", &path);
                structured(json!({"path": path, "appended": true}))
            }
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// A path, title or alias -> the note's path.
    fn find(&self, note: &str) -> Result<String, String> {
        let note = note.trim().trim_start_matches("[[").trim_end_matches("]]");
        if note.is_empty() {
            return Err("note is empty".into());
        }
        if note.ends_with(".md") && self.engine.read_note(note).is_ok() {
            return Ok(note.to_string());
        }
        self.engine.resolve(note.trim_end_matches(".md"), "").ok_or_else(|| format!("no note named \"{note}\""))
    }

    fn log(&self, tool: &str, path: &str) {
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let file = self.engine.app_dir().join("agent-log.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
            let _ = writeln!(f, "{}", json!({"ts_ms": ms, "tool": tool, "path": path}));
        }
    }
}

/// Where the running app tells `vault mcp` how to reach it: `{url, token, vault, pid}`.
/// `CERA_DISCOVERY` overrides the location (tests, several app instances).
pub fn discovery_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CERA_DISCOVERY") {
        return Some(PathBuf::from(p));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let dir = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cera")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("cera")
    };
    Some(dir.join("server.json"))
}

/// Record the running server for `vault mcp`. Owner-only permissions: the file holds the token.
pub fn publish(base_url: &str, token: &str, vault: &Path) -> std::io::Result<()> {
    let Some(path) = discovery_path() else { return Ok(()) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    let vault = vault.canonicalize().unwrap_or_else(|_| vault.to_path_buf());
    f.write_all(json!({"url": base_url, "token": token, "vault": vault, "pid": std::process::id()}).to_string().as_bytes())?;
    drop(f);
    std::fs::rename(tmp, path)
}

/// Remove the record if this process wrote it.
pub fn unpublish() {
    let Some(path) = discovery_path() else { return };
    let mine =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["pid"].as_u64()) == Some(std::process::id() as u64);
    if mine {
        let _ = std::fs::remove_file(path);
    }
}

fn capabilities() -> Value {
    json!({"tools": {"listChanged": false}, "resources": {"listChanged": false}})
}

fn server_info() -> Value {
    json!({"name": "cera", "title": "Cēra", "version": env!("CARGO_PKG_VERSION")})
}

fn msg(e: CoreError) -> String {
    match e {
        CoreError::Conflict { .. } => "the note changed while you were editing it; read it again and retry".into(),
        e => e.to_string(),
    }
}

pub fn note_uri(path: &str) -> String {
    let enc: String =
        path.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    format!("cera://note/{enc}")
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Search snippets are escaped HTML with <b> highlights; agents get plain text.
fn plain(html: &str) -> String {
    html.replace("<b>", "").replace("</b>", "").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

fn cut(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}\n\n[… truncated; read one section with `heading`]", &s[..end]), true)
}

/// ATX headings outside code fences: (byte offset of the line, level, text).
fn headings(md: &str) -> Vec<(usize, usize, String)> {
    let (mut out, mut at, mut fence) = (Vec::new(), 0, false);
    for line in md.split_inclusive('\n') {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") || t.trim_start().starts_with("~~~") {
            fence = !fence;
        } else if !fence {
            let level = t.bytes().take_while(|&b| b == b'#').count();
            if (1..=6).contains(&level) && t[level..].starts_with(' ') {
                out.push((at, level, t[level..].trim().trim_end_matches('#').trim().to_string()));
            }
        }
        at += line.len();
    }
    out
}

/// Byte range of a section: its heading line through the line before the next heading of the
/// same or a higher level. Matching ignores case.
pub fn section(md: &str, heading: &str) -> Option<(usize, usize)> {
    let hs = headings(md);
    let want = heading.trim().trim_start_matches('#').trim().to_lowercase();
    let i = hs.iter().position(|(_, _, t)| t.to_lowercase() == want)?;
    let (from, level, _) = hs[i];
    let to = hs[i + 1..].iter().find(|(_, l, _)| *l <= level).map(|h| h.0).unwrap_or(md.len());
    Some((from, to))
}

fn is_list_item(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.split_once(". ").is_some_and(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The note with `text` added at the end, or at the end of `heading`'s section (created if missing).
/// A list item added after a list continues that list instead of starting a new one.
pub fn append(md: &str, text: &str, heading: &str) -> String {
    let join = |before: &str, add: &str, after: &str| {
        let before = before.trim_end_matches('\n');
        let continues = before.lines().last().is_some_and(is_list_item) && add.lines().next().is_some_and(is_list_item);
        let sep = if before.is_empty() {
            ""
        } else if continues {
            "\n"
        } else {
            "\n\n"
        };
        let tail = if after.is_empty() { "\n".to_string() } else { format!("\n\n{after}") };
        format!("{before}{sep}{add}{tail}")
    };
    if heading.trim().is_empty() {
        return join(md, text, "");
    }
    match section(md, heading) {
        Some((_, to)) => join(&md[..to], text, &md[to..]),
        None => join(md, &format!("## {}\n\n{text}", heading.trim().trim_start_matches('#').trim()), ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_skip_code_and_stop_at_same_level() {
        let md = "# Plan\nintro\n## Goals\na\n```\n## not a heading\n```\nb\n### Sub\nc\n## Risks\nd\n";
        let (f, t) = section(md, "goals").unwrap();
        assert_eq!(&md[f..t], "## Goals\na\n```\n## not a heading\n```\nb\n### Sub\nc\n");
        assert_eq!(&md[section(md, "Risks").unwrap().0..], "## Risks\nd\n");
        assert!(section(md, "nope").is_none());
    }

    #[test]
    fn append_to_end_section_and_new_heading() {
        assert_eq!(append("a\n", "b", ""), "a\n\nb\n");
        assert_eq!(append("", "b", ""), "b\n");
        assert_eq!(append("## X\nx\n## Y\ny\n", "new", "x"), "## X\nx\n\nnew\n\n## Y\ny\n");
        assert_eq!(append("## X\nx\n", "new", "Later"), "## X\nx\n\n## Later\n\nnew\n");
        assert_eq!(append("## T\n- [x] a\n- [ ] b\n\n## Next\n", "- [ ] c", "t"), "## T\n- [x] a\n- [ ] b\n- [ ] c\n\n## Next\n");
        assert_eq!(append("1. one\n", "2. two", ""), "1. one\n2. two\n");
    }

    #[test]
    fn uris_round_trip() {
        let p = "Projects/Cera launch #1.md";
        let u = note_uri(p);
        assert_eq!(u, "cera://note/Projects/Cera%20launch%20%231.md");
        assert_eq!(percent_decode(u.strip_prefix("cera://note/").unwrap()), p);
    }

    #[test]
    fn snippets_become_plain_text() {
        assert_eq!(plain("a <b>b</b> &lt;c&gt; &amp;amp;"), "a b <c> &amp;");
    }
}
