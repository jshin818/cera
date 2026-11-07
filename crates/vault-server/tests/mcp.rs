//! MCP endpoint behaviour against the sample vault: handshake, tools, resources and writes.

use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use vault_core::{Engine, EngineOptions};
use vault_server::mcp::Mcp;

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

fn sample() -> (tempfile::TempDir, Engine) {
    let d = tempfile::tempdir().unwrap();
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../vault-core/tests/fixtures/sample-vault"), d.path());
    let e = Engine::open(d.path(), EngineOptions { watch: false, models_dir: None, ..Default::default() }).unwrap();
    assert!(e.wait_ready(Duration::from_secs(20)));
    e.flush();
    (d, e)
}

fn rpc(m: &Mcp, method: &str, params: Value) -> Value {
    m.handle(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})).expect("a reply")
}

fn call(m: &Mcp, tool: &str, args: Value) -> Value {
    rpc(m, "tools/call", json!({"name": tool, "arguments": args}))["result"].clone()
}

#[test]
fn handshake_both_revisions() {
    let (_d, e) = sample();
    let m = Mcp::new(e, false);
    let init = rpc(&m, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "cera");
    let unknown = rpc(&m, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(unknown["result"]["protocolVersion"], "2025-11-25");
    let disc = rpc(&m, "server/discover", json!({}));
    assert!(disc["result"]["supportedVersions"].as_array().unwrap().contains(&json!("2026-07-28")));
    assert_eq!(disc["result"]["resultType"], "complete");
    let bad = rpc(&m, "tools/list", json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "2001-01-01"}}));
    assert_eq!(bad["error"]["code"], -32022);
    assert!(m.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    assert_eq!(rpc(&m, "nope", json!({}))["error"]["code"], -32601);
    assert_eq!(m.handle(&json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}])).unwrap()["error"]["code"], -32600);
}

#[test]
fn read_tools() {
    let (_d, e) = sample();
    let m = Mcp::new(e, false);
    let names: Vec<String> =
        rpc(&m, "tools/list", json!({}))["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["context", "search", "read_note", "list_folder", "recent", "backlinks", "related", "tags"]);

    // Recall: numbered passages with sources, ready to cite.
    let c = call(&m, "context", json!({"question": "what did we decide to ship?"}));
    assert_eq!(c["isError"], false, "{c}");
    assert_eq!(c["structuredContent"]["passages"][0]["path"], "Projects/Beta.md", "{c}");
    assert!(c["content"][0]["text"].as_str().unwrap().starts_with("[1] Projects/Beta.md"), "{c}");
    assert_eq!(call(&m, "context", json!({"question": " "}))["isError"], true);

    let s = call(&m, "search", json!({"query": "gamma plan"}));
    assert_eq!(s["isError"], false);
    let hits = s["structuredContent"]["results"].as_array().unwrap();
    assert!(hits.iter().any(|h| h["path"] == "Gamma.md"), "{s}");
    assert!(hits.iter().all(|h| !h["excerpt"].as_str().unwrap_or("").contains("<b>")));

    // By alias, and one section only.
    let r = call(&m, "read_note", json!({"note": "Start"}));
    assert_eq!(r["structuredContent"]["note"]["path"], "Home.md");
    let sec = call(&m, "read_note", json!({"note": "Gamma", "heading": "plan"}));
    let body = sec["structuredContent"]["content"].as_str().unwrap();
    assert!(body.starts_with("## Plan") && !body.contains("# Gamma\n"), "{body}");
    assert_eq!(call(&m, "read_note", json!({"note": "Gamma", "heading": "Nope"}))["isError"], true);
    assert_eq!(call(&m, "read_note", json!({"note": "No such note"}))["isError"], true);

    let b = call(&m, "backlinks", json!({"note": "Alpha.md"}));
    assert!(!b["structuredContent"]["backlinks"].as_array().unwrap().is_empty(), "{b}");
    assert!(call(&m, "tags", json!({}))["structuredContent"]["tags"].as_array().unwrap().iter().any(|t| t["tag"] == "hub"));
    assert_eq!(call(&m, "tags", json!({"tag": "#hub"}))["structuredContent"]["notes"][0]["path"], "Home.md");
    assert!(call(&m, "list_folder", json!({}))["structuredContent"]["folders"].as_array().unwrap().len() > 1);

    // Writes are not offered without the write flag.
    assert_eq!(rpc(&m, "tools/call", json!({"name": "create_note", "arguments": {"title": "x", "content": "y"}}))["error"]["code"], -32602);
}

#[test]
fn resources() {
    let (_d, e) = sample();
    let m = Mcp::new(e, false);
    let list = rpc(&m, "resources/list", json!({}));
    assert!(list["result"]["resources"].as_array().unwrap().iter().any(|r| r["uri"] == "cera://note/Projects/Beta.md"));
    let read = rpc(&m, "resources/read", json!({"uri": "cera://note/Projects/Beta.md"}));
    assert!(read["result"]["contents"][0]["text"].as_str().unwrap().contains("We decided to ship"));
    assert_eq!(rpc(&m, "resources/read", json!({"uri": "cera://note/Nope.md"}))["error"]["code"], -32602);
    assert_eq!(rpc(&m, "resources/read", json!({"uri": "cera://note/assets/diagram.png"}))["error"]["code"], -32602);
    assert_eq!(rpc(&m, "resources/read", json!({"uri": "cera://note/.vault/agent-log.jsonl"}))["error"]["code"], -32602);
}

#[test]
fn writes_go_to_inbox_and_are_logged() {
    let (d, e) = sample();
    let m = Mcp::new(e.clone(), true);
    let c = call(&m, "create_note", json!({"title": "Meeting notes", "content": "Talked about [[Alpha]].", "tags": ["#meeting", "follow up"]}));
    assert_eq!(c["structuredContent"]["path"], "Inbox/Meeting notes.md", "{c}");
    let text = std::fs::read_to_string(d.path().join("Inbox/Meeting notes.md")).unwrap();
    assert_eq!(text, "---\ntags: [meeting, follow-up]\n---\nTalked about [[Alpha]].");
    // A taken name gets a number; folder "" is the top level.
    assert_eq!(call(&m, "create_note", json!({"title": "Meeting notes", "content": "again"}))["structuredContent"]["path"], "Inbox/Meeting notes 1.md");
    assert_eq!(call(&m, "create_note", json!({"title": "Top", "content": "t", "folder": ""}))["structuredContent"]["path"], "Top.md");

    let a = call(&m, "append_to_note", json!({"note": "Gamma", "heading": "Plan", "text": "- ship it"}));
    assert_eq!(a["isError"], false, "{a}");
    let g = std::fs::read_to_string(d.path().join("Gamma.md")).unwrap();
    assert!(g.ends_with("```\n\n- ship it\n"), "{g}");
    e.flush();
    assert!(e.search_text("ship", 5).unwrap().iter().any(|h| h.note.path == "Gamma.md"), "appended text is searchable");

    let log = std::fs::read_to_string(d.path().join(".vault/agent-log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 4);
    assert!(log.lines().last().unwrap().contains("\"append_to_note\""));
}
