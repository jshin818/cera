//! Answers from the notes. Recall (vault-core) picks the passages; a model writes the answer and
//! cites them as [n]. Three providers: Claude through the Messages API with the user's key; a
//! hosted OpenAI-compatible API with the user's key (Groq, OpenRouter, Together, OpenAI, Mistral,
//! DeepSeek, or any other URL); and a local OpenAI-compatible server (Ollama, LM Studio). With none,
//! the Ask view shows the sources alone.
//!
//! Settings live in the app's config folder, never in the vault, so a synced or shared vault never
//! carries a key; keys are written 0600 and never sent back to the page. `ANTHROPIC_API_KEY` works
//! too. Local URLs must be on this machine; hosted URLs must be https.

use axum::response::sse::Event;
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use vault_core::{Engine, Recall, RecallOptions};

pub const CLAUDE_URL: &str = "https://api.anthropic.com/v1/messages";
pub const ANTHROPIC_VERSION: &str = "2026-10-21";
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_LOCAL_URL: &str = "http://localhost:11434";
/// Groq: open models (Llama, Qwen, GPT-OSS), fast and with a free tier.
pub const DEFAULT_API_URL: &str = "https://api.groq.com/openai/v1";
/// Models that can fall back to another model server-side when they are overloaded.
const FALLBACK_MODELS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5"];
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// auto (Claude if it has a key, else the hosted API if it has a key and a model, else a local
    /// server if one answers) | claude | api | local | off
    pub provider: String,
    pub claude_model: String,
    /// Base URL of a hosted OpenAI-compatible API, up to the version (`…/v1`).
    pub api_url: String,
    pub api_model: String,
    pub local_url: String,
    /// Empty = the first model the local server lists.
    pub local_model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub anthropic_api_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "auto".into(),
            claude_model: DEFAULT_CLAUDE_MODEL.into(),
            api_url: DEFAULT_API_URL.into(),
            api_model: String::new(),
            local_url: DEFAULT_LOCAL_URL.into(),
            local_model: String::new(),
            anthropic_api_key: String::new(),
            api_key: String::new(),
        }
    }
}

/// `<config dir>/Cera/ai.json`: the same folder the desktop app keeps its recent vaults in.
/// `CERA_CONFIG_DIR` overrides the folder (tests, a second profile).
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CERA_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("ai.json"));
    }
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Cera"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("cera"))
    }?;
    Some(base.join("ai.json"))
}

/// Only hosts on this machine count as local.
pub fn is_loopback_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("http://").or_else(|| u.strip_prefix("https://")) else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// A hosted API gets the key, so it must be https; plain http only on this machine.
pub fn is_api_url(u: &str) -> bool {
    let host_ok = |rest: &str| rest.split(['/', '?', '#']).next().is_some_and(|h| !h.is_empty() && !h.contains('@'));
    u.strip_prefix("https://").is_some_and(host_ok) || is_loopback_url(u)
}

pub struct Ai {
    path: Option<PathBuf>,
    cfg: Mutex<AiConfig>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    Claude { key: String, model: String },
    Api { url: String, key: String, model: String },
    Local { url: String, model: String },
}

impl Provider {
    fn label(&self) -> Value {
        match self {
            Provider::Claude { model, .. } => json!({"provider": "claude", "model": model}),
            Provider::Api { url, model, .. } => json!({"provider": "api", "model": model, "url": url}),
            Provider::Local { url, model } => json!({"provider": "local", "model": model, "url": url}),
        }
    }
}

impl Ai {
    /// Settings from `path` (missing or unreadable = defaults). `None` keeps them in memory only.
    pub fn load(path: Option<PathBuf>) -> Self {
        let cfg = path.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).read_timeout(Duration::from_secs(120)).build().unwrap_or_default();
        Self { path, cfg: Mutex::new(cfg), http }
    }

    fn key(&self) -> (String, Option<&'static str>) {
        let k = self.cfg.lock().anthropic_api_key.clone();
        if !k.is_empty() {
            return (k, Some("saved"));
        }
        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(k) if !k.trim().is_empty() => (k.trim().to_string(), Some("env")),
            _ => (String::new(), None),
        }
    }

    /// What the page may see: everything but the key.
    pub fn public(&self) -> Value {
        let c = self.cfg.lock().clone();
        let (_, source) = self.key();
        json!({"provider": c.provider, "claude_model": c.claude_model, "local_url": c.local_url, "local_model": c.local_model,
               "has_key": source.is_some(), "key_source": source,
               "api_url": c.api_url, "api_model": c.api_model, "has_api_key": !c.api_key.is_empty()})
    }

    /// Apply the fields present in `patch` and save. An empty `anthropic_api_key` or `api_key`
    /// forgets that key.
    pub fn update(&self, patch: &Value) -> Result<Value, String> {
        let mut c = self.cfg.lock().clone();
        let s = |k: &str| patch.get(k).and_then(Value::as_str).map(|v| v.trim().to_string());
        if let Some(p) = s("provider") {
            if !["auto", "claude", "api", "local", "off"].contains(&p.as_str()) {
                return Err(format!("unknown provider \"{p}\""));
            }
            c.provider = p;
        }
        if let Some(m) = s("claude_model") {
            c.claude_model = if m.is_empty() { DEFAULT_CLAUDE_MODEL.into() } else { m };
        }
        if let Some(u) = s("local_url") {
            let u = if u.is_empty() { DEFAULT_LOCAL_URL.to_string() } else { u.trim_end_matches('/').to_string() };
            if !is_loopback_url(&u) {
                return Err("the local model URL must be on this machine (localhost, 127.0.0.1 or [::1])".into());
            }
            c.local_url = u;
        }
        if let Some(u) = s("api_url") {
            let u = if u.is_empty() { DEFAULT_API_URL.to_string() } else { u.trim_end_matches('/').to_string() };
            if !is_api_url(&u) {
                return Err("the API URL must start with https:// (or be on this machine)".into());
            }
            if u != c.api_url {
                c.api_model.clear();
            }
            c.api_url = u;
        }
        if let Some(m) = s("api_model") {
            c.api_model = m;
        }
        if let Some(k) = s("api_key") {
            c.api_key = k;
        }
        if let Some(m) = s("local_model") {
            c.local_model = m;
        }
        if let Some(k) = s("anthropic_api_key") {
            c.anthropic_api_key = k;
        }
        if let Some(p) = &self.path {
            save(p, &c).map_err(|e| format!("couldn't save AI settings: {e}"))?;
        }
        *self.cfg.lock() = c;
        Ok(self.public())
    }

    /// Model ids a local server offers (`GET {url}/v1/models`); `None` when it doesn't answer.
    pub async fn local_models(&self, url: &str) -> Option<Vec<String>> {
        if !is_loopback_url(url) {
            return None;
        }
        let r = self.http.get(format!("{}/v1/models", url.trim_end_matches('/'))).timeout(Duration::from_millis(1500)).send().await.ok()?;
        let v: Value = r.error_for_status().ok()?.json().await.ok()?;
        Some(v["data"].as_array()?.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect())
    }

    /// Model ids a hosted API offers to the saved key (`GET {api_url}/models`).
    pub async fn api_models(&self, url: Option<&str>) -> Result<Vec<String>, String> {
        let c = self.cfg.lock().clone();
        let url = url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or(c.api_url);
        if !is_api_url(&url) {
            return Err("The API URL must start with https://.".into());
        }
        if c.api_key.is_empty() {
            return Err("Add the API key first.".into());
        }
        let r = self
            .http
            .get(format!("{url}/models"))
            .bearer_auth(&c.api_key)
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .map_err(|_| format!("Nothing answered at {url}."))?;
        match r.status().as_u16() {
            401 | 403 => return Err("The API rejected this key.".into()),
            c if c >= 400 => return Err(format!("The API answered HTTP {c}.")),
            _ => {}
        }
        let v: Value = r.json().await.map_err(|_| "The API's model list wasn't JSON.".to_string())?;
        let mut ids: Vec<String> = v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect();
        ids.sort();
        Ok(ids)
    }

    /// The provider to answer with now, or why there is none.
    pub async fn pick(&self) -> Result<Provider, String> {
        let c = self.cfg.lock().clone();
        let (key, _) = self.key();
        let claude = || Provider::Claude { key: key.clone(), model: c.claude_model.clone() };
        let api = || Provider::Api { url: c.api_url.clone(), key: c.api_key.clone(), model: c.api_model.clone() };
        let api_ready = !c.api_key.is_empty() && !c.api_model.is_empty();
        let local = async {
            let models = self.local_models(&c.local_url).await;
            match models {
                Some(m) if !c.local_model.is_empty() || !m.is_empty() => {
                    Ok(Provider::Local { url: c.local_url.clone(), model: if c.local_model.is_empty() { m[0].clone() } else { c.local_model.clone() } })
                }
                Some(_) => Err(format!("The local server at {} has no models loaded.", c.local_url)),
                None => Err(format!("No local model server answered at {}.", c.local_url)),
            }
        };
        match c.provider.as_str() {
            "off" => Err("Answers are turned off; showing sources only.".into()),
            "claude" if key.is_empty() => Err("Add an Anthropic API key in Settings → AI to answer with Claude.".into()),
            "claude" => Ok(claude()),
            "api" if c.api_key.is_empty() => Err("Add an API key in Settings → AI to answer with it.".into()),
            "api" if c.api_model.is_empty() => Err("Pick a model for the API in Settings → AI.".into()),
            "api" => Ok(api()),
            "local" => local.await,
            _ if !key.is_empty() => Ok(claude()),
            _ if api_ready => Ok(api()),
            _ => local
                .await
                .map_err(|_| "No answer model is set up, so here are the sources. Add an API key or start Ollama / LM Studio in Settings → AI.".into()),
        }
    }
}

fn save(p: &std::path::Path, c: &AiConfig) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(c).unwrap_or_default().as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, p)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskRequest {
    pub question: String,
    #[serde(default)]
    pub history: Vec<Turn>,
    pub limit: Option<usize>,
}

/// One step of a streamed answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Stop(String),
    Error(String),
}

/// Splits a byte stream into server-sent-event `data:` payloads.
#[derive(Default)]
pub struct SseData {
    buf: String,
    cur: Vec<String>,
}

impl SseData {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(i) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=i).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.cur.is_empty() {
                    out.push(self.cur.join("\n"));
                    self.cur.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                self.cur.push(d.strip_prefix(' ').unwrap_or(d).to_string());
            }
        }
        out
    }
}

/// A Messages API stream event.
pub fn claude_event(data: &str) -> Option<Delta> {
    let v: Value = serde_json::from_str(data).ok()?;
    match v["type"].as_str()? {
        "content_block_delta" if v["delta"]["type"] == "text_delta" => Some(Delta::Text(v["delta"]["text"].as_str()?.to_string())),
        "message_delta" => v["delta"]["stop_reason"].as_str().map(|s| Delta::Stop(s.to_string())),
        "error" => Some(Delta::Error(v["error"]["message"].as_str().unwrap_or("Claude returned an error").to_string())),
        _ => None,
    }
}

/// An OpenAI-compatible chat-completions stream event.
pub fn openai_event(data: &str) -> Option<Delta> {
    if data.trim() == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    if let Some(e) = v.get("error") {
        return Some(Delta::Error(e["message"].as_str().or(e.as_str()).unwrap_or("the model returned an error").to_string()));
    }
    let c = &v["choices"][0];
    if let Some(t) = c["delta"]["content"].as_str().filter(|t| !t.is_empty()) {
        return Some(Delta::Text(t.to_string()));
    }
    c["finish_reason"].as_str().map(|s| Delta::Stop(s.to_string()))
}

pub fn system_prompt(today: &str) -> String {
    format!(
        "You answer questions from the user's own notes. Use only the numbered sources you are given. Cite every claim with \
the source number in square brackets right after it, like [2] or [1][3]. If the sources don't answer the question, say so \
plainly and say what is missing; don't fill gaps from general knowledge unless the user asks, and then mark that part as not \
from their notes. If sources disagree, or a gap is listed, mention it. Lead with the answer, keep it short, use Markdown, and \
quote only a few words at a time. Today is {today}."
    )
}

/// The conversation for the model: earlier turns as plain text, then the sources and the question.
pub fn messages(history: &[Turn], recall: &Recall, question: &str) -> Vec<Value> {
    let mut m: Vec<Value> = history
        .iter()
        .rev()
        .take(8)
        .rev()
        .filter(|t| (t.role == "user" || t.role == "assistant") && !t.content.trim().is_empty())
        .map(|t| json!({"role": t.role, "content": t.content}))
        .collect();
    // The API wants turns to alternate and to start with the user.
    while m.first().is_some_and(|t| t["role"] != "user") {
        m.remove(0);
    }
    if m.last().is_some_and(|t| t["role"] == "user") {
        m.pop();
    }
    let ctx = recall.context_text();
    let ctx = if ctx.is_empty() { "(no passages matched)".to_string() } else { ctx };
    m.push(json!({"role": "user", "content": format!("<sources>\n{ctx}</sources>\n\nQuestion: {question}")}));
    m
}

/// The recall query: a short follow-up ("and the pricing?") borrows the previous question's words.
pub fn recall_query(history: &[Turn], question: &str) -> String {
    let short = question.split_whitespace().count() < 6;
    match history.iter().rev().find(|t| t.role == "user") {
        Some(prev) if short => format!("{} {}", prev.content, question),
        _ => question.to_string(),
    }
}

fn ev(name: &str, data: Value) -> Event {
    Event::default().event(name).json_data(data).unwrap_or_default()
}

async fn fail(tx: &Sender<Event>, message: String) {
    let _ = tx.send(ev("error", json!({"message": message}))).await;
}

/// Recall, then stream an answer: `sources`, `model`, `delta`…, then `done` or `error`.
/// Stops as soon as the page goes away (the channel closes).
pub async fn run_ask(engine: Engine, ai: std::sync::Arc<Ai>, req: AskRequest, tx: Sender<Event>) {
    let question = req.question.trim().to_string();
    if question.is_empty() {
        return fail(&tx, "Ask a question first.".into()).await;
    }
    let q = recall_query(&req.history, &question);
    let opts = RecallOptions { limit: req.limit.unwrap_or(8).clamp(1, 20), ..Default::default() };
    let recall = match tokio::task::spawn_blocking(move || engine.recall(&q, &opts)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return fail(&tx, e.to_string()).await,
        Err(e) => return fail(&tx, e.to_string()).await,
    };
    if tx.send(ev("sources", serde_json::to_value(&recall).unwrap_or(Value::Null))).await.is_err() {
        return;
    }
    if recall.passages.is_empty() {
        let _ = tx.send(ev("done", json!({"answered": false, "note": "Nothing in your notes matches this, so there is nothing to answer from."}))).await;
        return;
    }
    let provider = match ai.pick().await {
        Ok(p) => p,
        Err(note) => {
            let _ = tx.send(ev("done", json!({"answered": false, "note": note}))).await;
            return;
        }
    };
    if tx.send(ev("model", provider.label())).await.is_err() {
        return;
    }
    let today = vault_core::ymd(vault_core::now_ms());
    let msgs = messages(&req.history, &recall, &question);
    let system = system_prompt(&today);
    let (resp, parse): (Result<reqwest::Response, reqwest::Error>, fn(&str) -> Option<Delta>) = match &provider {
        Provider::Claude { key, model } => {
            let mut body = json!({"model": model, "max_tokens": 16000, "stream": true, "system": system, "messages": msgs, "output_config": {"effort": "low"}});
            let mut r = ai.http.post(CLAUDE_URL).header("x-api-key", key).header("anthropic-version", ANTHROPIC_VERSION);
            if FALLBACK_MODELS.contains(&model.as_str()) {
                body["fallbacks"] = json!("default");
                r = r.header("anthropic-beta", FALLBACK_BETA);
            }
            (r.json(&body).send().await, claude_event)
        }
        Provider::Api { url, key, model } => {
            (ai.http.post(format!("{url}/chat/completions")).bearer_auth(key).json(&chat_body(model, system, msgs)).send().await, openai_event)
        }
        Provider::Local { url, model } => (ai.http.post(format!("{url}/v1/chat/completions")).json(&chat_body(model, system, msgs)).send().await, openai_event),
    };
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let code = r.status();
            let body: Value = r.json().await.unwrap_or(Value::Null);
            let msg = body["error"]["message"].as_str().or(body["error"].as_str()).map(String::from).unwrap_or_else(|| format!("HTTP {code}"));
            return fail(&tx, friendly(&provider, code.as_u16(), &msg)).await;
        }
        Err(e) => {
            return fail(&tx, format!("Couldn't reach the model: {e}")).await;
        }
    };
    let mut stream = resp.bytes_stream();
    let mut sse = SseData::default();
    let mut stop = String::from("end_turn");
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return fail(&tx, format!("The answer stream broke off: {e}")).await,
        };
        for data in sse.push(&chunk) {
            match parse(&data) {
                Some(Delta::Text(t)) => {
                    if tx.send(ev("delta", json!({"text": t}))).await.is_err() {
                        return;
                    }
                }
                Some(Delta::Stop(s)) => stop = s,
                Some(Delta::Error(m)) => return fail(&tx, m).await,
                None => {}
            }
        }
    }
    let note = match stop.as_str() {
        "refusal" => Some("The model declined to answer this."),
        "max_tokens" | "length" => Some("The answer was cut off at the length limit."),
        _ => None,
    };
    let _ = tx.send(ev("done", json!({"answered": true, "stop_reason": stop, "note": note}))).await;
}

/// An OpenAI-style chat-completions request: the system prompt goes first as a message.
fn chat_body(model: &str, system: String, msgs: Vec<Value>) -> Value {
    let mut all = vec![json!({"role": "system", "content": system})];
    all.extend(msgs);
    json!({"model": model, "stream": true, "messages": all})
}

fn friendly(p: &Provider, code: u16, msg: &str) -> String {
    match (p, code) {
        (Provider::Claude { .. }, 401) => "Claude rejected the API key. Check it in Settings → AI.".into(),
        (Provider::Claude { .. }, 429) => "Claude is rate-limiting this key; try again in a moment.".into(),
        (Provider::Claude { .. }, 529) => "Claude is overloaded right now; try again in a moment.".into(),
        (Provider::Api { .. }, 401 | 403) => "The API rejected the key. Check it in Settings → AI.".into(),
        (Provider::Api { .. }, 429) => "The API is rate-limiting this key; try again in a moment.".into(),
        (Provider::Api { model, .. }, 404) => format!("The API doesn't offer the model \"{model}\". Pick another in Settings → AI."),
        (Provider::Local { model, .. }, 404) => format!("The local server doesn't have the model \"{model}\". Pick another in Settings → AI."),
        _ => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_splits_events_across_chunks() {
        let mut s = SseData::default();
        assert!(s.push(b"event: content_block_delta\ndata: {\"a\":").is_empty());
        assert_eq!(s.push(b"1}\r\n\r\ndata: [DONE]\n\n"), vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn claude_stream_events() {
        assert_eq!(
            claude_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi [1]"}}"#),
            Some(Delta::Text("Hi [1]".into()))
        );
        assert_eq!(claude_event(r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#), Some(Delta::Stop("refusal".into())));
        assert_eq!(claude_event(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#), Some(Delta::Error("Overloaded".into())));
        assert_eq!(claude_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"fallback"}}"#), None);
        assert_eq!(claude_event(r#"{"type":"ping"}"#), None);
    }

    #[test]
    fn openai_stream_events() {
        assert_eq!(openai_event(r#"{"choices":[{"delta":{"content":"Yes"},"finish_reason":null}]}"#), Some(Delta::Text("Yes".into())));
        assert_eq!(openai_event(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#), Some(Delta::Stop("length".into())));
        assert_eq!(openai_event("[DONE]"), None);
        assert_eq!(openai_event(r#"{"error":{"message":"model not found"}}"#), Some(Delta::Error("model not found".into())));
    }

    #[test]
    fn local_means_this_machine() {
        assert!(is_loopback_url("http://localhost:11434"));
        assert!(is_loopback_url("http://127.0.0.1:1234/v1"));
        assert!(is_loopback_url("http://[::1]:8080"));
        assert!(!is_loopback_url("http://localhost.evil.com"));
        assert!(!is_loopback_url("http://10.0.0.2:11434"));
        assert!(!is_loopback_url("ftp://localhost"));
    }

    #[test]
    fn hosted_apis_need_https() {
        assert!(is_api_url("https://api.groq.com/openai/v1"));
        assert!(is_api_url("http://127.0.0.1:8080/v1"));
        assert!(!is_api_url("http://api.groq.com/openai/v1"));
        assert!(!is_api_url("https://"));
        assert!(!is_api_url("https://user@evil.com/v1"));
    }

    #[test]
    fn follow_ups_borrow_the_previous_question() {
        let h = vec![
            Turn { role: "user".into(), content: "What did we decide about Beta?".into() },
            Turn { role: "assistant".into(), content: "To ship [1].".into() },
        ];
        assert_eq!(recall_query(&h, "and when?"), "What did we decide about Beta? and when?");
        assert_eq!(recall_query(&h, "What are the open tasks for the Alpha project?"), "What are the open tasks for the Alpha project?");
        assert_eq!(recall_query(&[], "and when?"), "and when?");
    }

    #[test]
    fn settings_keep_the_key_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Cera/ai.json");
        let ai = Ai::load(Some(p.clone()));
        let v = ai.update(&json!({"provider": "claude", "anthropic_api_key": " sk-test "})).unwrap();
        assert_eq!(v["has_key"], true);
        assert_eq!(v["key_source"], "saved");
        assert!(v.get("anthropic_api_key").is_none());
        assert!(ai.update(&json!({"local_url": "http://192.168.1.5:11434"})).is_err());
        assert!(ai.update(&json!({"provider": "gpt"})).is_err());
        let v = ai.update(&json!({"provider": "api", "api_key": "gsk-test", "api_model": "llama-3.3-70b-versatile"})).unwrap();
        assert_eq!(v["has_api_key"], true);
        assert!(v.get("api_key").is_none());
        assert_eq!(v["api_url"], DEFAULT_API_URL);
        assert!(ai.update(&json!({"api_url": "http://api.groq.com/openai/v1"})).is_err());
        // A new URL is a new provider, so its model has to be picked again.
        let v = ai.update(&json!({"api_url": "https://openrouter.ai/api/v1/"})).unwrap();
        assert_eq!(v["api_url"], "https://openrouter.ai/api/v1");
        assert_eq!(v["api_model"], "");
        ai.update(&json!({"provider": "claude"})).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Reloads from disk; an empty key forgets it.
        let ai = Ai::load(Some(p));
        assert_eq!(ai.public()["provider"], "claude");
        assert_eq!(ai.cfg.lock().anthropic_api_key, "sk-test");
        ai.update(&json!({"anthropic_api_key": ""})).unwrap();
        assert!(ai.cfg.lock().anthropic_api_key.is_empty());
    }
}
