//! `vault mcp`: the Model Context Protocol over stdio, for AI agents that launch a command.
//!
//! The vault's cache allows one process at a time, so when the Cēra app has the vault open this
//! forwards each message to the app's `/mcp` endpoint (found through `server.json`). Otherwise it
//! opens the vault itself. stdout carries only JSON-RPC; everything else goes to stderr.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use vault_core::{Engine, EngineOptions};
use vault_server::mcp::Mcp;

struct Remote {
    addr: String,
    token: String,
    write: bool,
}

impl Remote {
    /// POST one message to the app. Ok(None) = accepted with no reply (a notification).
    fn post(&self, body: &str) -> Result<Option<String>> {
        let mut s = TcpStream::connect(&self.addr)?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        let write = if self.write { "x-vault-mcp-write: 1\r\n" } else { "" };
        write!(
            s,
            "POST /mcp HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nAuthorization: Bearer {}\r\n{write}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            self.token,
            body.len()
        )?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let raw = String::from_utf8_lossy(&raw);
        let (head, body) = raw.split_once("\r\n\r\n").context("malformed HTTP response")?;
        let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        match status {
            200 => Ok(Some(body.trim().to_string())),
            202 => Ok(None),
            _ => bail!("Cēra answered {status}: {}", body.trim()),
        }
    }
}

/// The running app's server, if it has `vault` open (any vault when `vault` is None) and answers.
fn find_app(vault: Option<&Path>, write: bool) -> Option<Remote> {
    let path = vault_server::mcp::discovery_path()?;
    let d: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if let Some(v) = vault {
        let want = v.canonicalize().ok()?;
        if Path::new(d["vault"].as_str()?) != want {
            return None;
        }
    }
    let r = Remote { addr: d["url"].as_str()?.strip_prefix("http://")?.trim_end_matches('/').to_string(), token: d["token"].as_str()?.to_string(), write };
    r.post(&json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}).to_string()).ok()?;
    Some(r)
}

pub fn run(vault: Option<PathBuf>, write: bool) -> Result<()> {
    enum Backend {
        App(Remote),
        Local(Mcp),
    }
    let backend = match find_app(vault.as_deref(), write) {
        Some(r) => {
            eprintln!("vault mcp: using the running Cēra app at {}", r.addr);
            Backend::App(r)
        }
        None => {
            let Some(v) = vault else { bail!("Cēra isn't running. Start it, or pass the vault folder: vault mcp <vault>") };
            let e = Engine::open(&v, EngineOptions::default()).with_context(|| format!("opening {} (is another program using this vault?)", v.display()))?;
            if !e.wait_ready(Duration::from_secs(30)) {
                eprintln!("vault mcp: still indexing; early results may be incomplete");
            }
            eprintln!("vault mcp: serving {}", v.display());
            Backend::Local(Mcp::new(e, write))
        }
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}}).to_string()),
            Ok(msg) => match &backend {
                Backend::Local(m) => m.handle(&msg).map(|v| v.to_string()),
                Backend::App(r) => match r.post(&line) {
                    Ok(reply) => reply,
                    Err(e) => msg
                        .get("id")
                        .map(|id| json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": format!("Cēra app unreachable: {e}")}}).to_string()),
                },
            },
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    if let Backend::Local(m) = backend {
        m.engine.shutdown();
    }
    Ok(())
}
