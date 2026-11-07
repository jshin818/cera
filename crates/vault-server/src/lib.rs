//! vault-server: a localhost HTTP shell over vault-core. It serves the UI (embedded from
//! `ui/dist`) and a JSON API. The Tauri app and `vault serve` both use it.
//!
//! Every engine call runs on tokio's blocking pool, so request handling never stalls on I/O.
//! When a token is set, `/api/*` and `/file/*` require it (cookie set by `/?t=<token>`, or the
//! `x-vault-token` header), so other local processes can't read the vault through the port.
//! `/mcp` is the Model Context Protocol endpoint for AI agents (see [`mcp`]); it also accepts
//! `Authorization: Bearer <token>`. `/api/ask` streams an answer from the notes with its sources
//! (see [`ai`]).

pub mod ai;
pub mod mcp;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use vault_core::{CoreError, Engine, EngineOptions};

#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist"]
struct Assets;

pub use vault_core::Engine as VaultEngine;

pub struct ServeOptions {
    pub vault: PathBuf,
    /// An engine already open on `vault`; `None` opens it here.
    pub engine: Option<Engine>,
    pub addr: String,
    pub open_browser: bool,
    /// Require this token on API calls. `None` = no auth (still bound to localhost).
    pub token: Option<String>,
    /// Called with the bound address and the URL to open once the server is listening.
    pub on_ready: Option<Box<dyn FnOnce(SocketAddr, String) + Send>>,
    /// Lets the page switch vaults (the desktop app). `None` hides the switcher.
    pub host: Option<Arc<dyn VaultHost>>,
    /// Stop serving when this resolves. `None` = on Ctrl-C.
    pub shutdown: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
}

/// What the desktop app does for the vault switcher. Every method may block; the server calls
/// them off the async runtime.
pub trait VaultHost: Send + Sync {
    /// Vaults opened before, most recent first (may include folders that no longer exist).
    fn recent(&self) -> Vec<PathBuf>;
    /// Drop a vault from the recent list.
    fn forget(&self, vault: &std::path::Path);
    /// Ask for a folder with the system picker. `None` = cancelled.
    fn pick_folder(&self) -> Option<PathBuf>;
    /// Serve `engine` (already open on `vault`) in place of this server, which should then stop.
    fn switch_to(&self, vault: PathBuf, engine: Engine);
}

#[derive(Clone)]
struct App {
    engine: Engine,
    token: Option<Arc<str>>,
    host: Option<Arc<dyn VaultHost>>,
    /// Flips to true when the server is stopping, so open event streams end and the graceful
    /// shutdown doesn't wait on them forever.
    closing: Option<tokio::sync::watch::Receiver<bool>>,
    ai: Arc<ai::Ai>,
}

pub fn random_token() -> String {
    use rand::Rng;
    let mut r = rand::rng();
    (0..32).map(|_| format!("{:x}", r.random_range(0..16u8))).collect()
}

/// Open the vault and serve until Ctrl-C (or `o.shutdown`).
pub async fn serve(o: ServeOptions) -> anyhow::Result<()> {
    let engine = match o.engine {
        Some(e) => e,
        None => {
            let vault = o.vault.clone();
            tokio::task::spawn_blocking(move || Engine::open(&vault, EngineOptions::default())).await??
        }
    };
    let listener = tokio::net::TcpListener::bind(&o.addr).await?;
    let addr = listener.local_addr()?;
    let url = match &o.token {
        Some(t) => format!("http://{addr}/?t={t}"),
        None => format!("http://{addr}/"),
    };
    eprintln!("Vault: serving {} at {url}", engine.root().display());
    if o.open_browser {
        open_url(&url);
    }
    if let Some(cb) = o.on_ready {
        cb(addr, url.clone());
    }
    let (closing_tx, closing) = tokio::sync::watch::channel(false);
    let ai = Arc::new(ai::Ai::load(ai::default_config_path()));
    let app = build_router(engine.clone(), o.token, o.host, Some(closing), ai);
    let stop = o.shutdown;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            match stop {
                Some(f) => f.await,
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
            let _ = closing_tx.send(true);
        })
        .await?;
    eprintln!("Vault: closing {}", engine.root().display());
    tokio::task::spawn_blocking(move || engine.shutdown()).await?;
    Ok(())
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", url]).spawn();
}

pub fn router(engine: Engine, token: Option<String>) -> Router {
    router_with_host(engine, token, None)
}

pub fn router_with_host(engine: Engine, token: Option<String>, host: Option<Arc<dyn VaultHost>>) -> Router {
    build_router(engine, token, host, None, Arc::new(ai::Ai::load(ai::default_config_path())))
}

/// A router whose AI settings come from (and are saved to) `ai_config`; `None` keeps them in memory.
pub fn router_with_ai(engine: Engine, token: Option<String>, ai_config: Option<PathBuf>) -> Router {
    build_router(engine, token, None, None, Arc::new(ai::Ai::load(ai_config)))
}

fn build_router(
    engine: Engine,
    token: Option<String>,
    host: Option<Arc<dyn VaultHost>>,
    closing: Option<tokio::sync::watch::Receiver<bool>>,
    ai: Arc<ai::Ai>,
) -> Router {
    let state = App { engine, token: token.map(Arc::from), host, closing, ai };
    let api = Router::new()
        .route("/api/boot", get(boot))
        .route("/api/vaults", get(vaults_get))
        .route("/api/vaults/open", post(vaults_open))
        .route("/api/vaults/forget", post(vaults_forget))
        .route("/api/status", get(status))
        .route("/api/dir", get(dir))
        .route("/api/recent", get(recent))
        .route("/api/note", get(note_get).put(note_put).post(note_create))
        .route("/api/rename", post(rename))
        .route("/api/delete", post(delete))
        .route("/api/folder", post(folder_create))
        .route("/api/folder/rename", post(folder_rename))
        .route("/api/folder/delete", post(folder_delete))
        .route("/api/switch", get(switch))
        .route("/api/search", get(search))
        .route("/api/recall", get(recall))
        .route("/api/ask", post(ask))
        .route("/api/ai", get(ai_get).put(ai_put))
        .route("/api/ai/models", get(ai_models))
        .route("/api/resolve", get(resolve))
        .route("/api/backlinks", get(backlinks))
        .route("/api/mentions", get(mentions))
        .route("/api/outgoing-mentions", get(outgoing))
        .route("/api/link-mention", post(link_mention))
        .route("/api/related", get(related))
        .route("/api/suggest", get(suggest))
        .route("/api/dupes", get(dupes))
        .route("/api/tags", get(tags))
        .route("/api/tag", get(tag))
        .route("/api/clusters", get(clusters))
        .route("/api/graph", get(graph))
        .route("/api/stats", get(stats))
        .route("/api/workspace", get(ws_get).put(ws_put))
        .route("/api/attachment", post(attachment))
        .route("/api/rescan", post(rescan))
        .route("/api/events", get(events))
        .route("/file/{*path}", get(file))
        .route("/mcp", post(mcp_post).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new().merge(api).route("/", get(index)).route("/{*path}", get(asset)).layer(tower_http::compression::CompressionLayer::new()).with_state(state)
}

async fn auth(State(app): State<App>, req: Request<Body>, next: Next) -> Response {
    if let Some(tok) = &app.token {
        let headers = req.headers();
        let ok = headers.get("x-vault-token").and_then(|v| v.to_str().ok()) == Some(tok)
            || headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) == Some(tok)
            || headers
                .get_all(header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|c| c.split(';'))
                .any(|kv| kv.trim().strip_prefix("vt=") == Some(tok));
        if !ok {
            return (StatusCode::UNAUTHORIZED, "missing or wrong token").into_response();
        }
    }
    next.run(req).await
}

struct ApiError(CoreError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, kind) = match &self.0 {
            CoreError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            CoreError::Conflict { .. } => (StatusCode::CONFLICT, "conflict"),
            CoreError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            CoreError::Exists(_) => (StatusCode::CONFLICT, "exists"),
            CoreError::Other(_) => (StatusCode::INTERNAL_SERVER_ERROR, "error"),
        };
        (code, Json(json!({"error": kind, "message": self.0.to_string()}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Run a blocking engine call on the blocking pool and JSON-encode its result.
async fn blocking<T: serde::Serialize + Send + 'static>(app: &App, f: impl FnOnce(&Engine) -> vault_core::Result<T> + Send + 'static) -> ApiResult {
    let e = app.engine.clone();
    match tokio::task::spawn_blocking(move || f(&e)).await {
        Ok(Ok(v)) => Ok(Json(serde_json::to_value(v).unwrap_or(Value::Null))),
        Ok(Err(e)) => Err(ApiError(e)),
        Err(e) => Err(ApiError(CoreError::Other(e.to_string()))),
    }
}

#[derive(Deserialize)]
struct PathQ {
    path: String,
}

#[derive(Deserialize)]
struct LimitQ {
    q: Option<String>,
    limit: Option<usize>,
    mode: Option<String>,
}

async fn index(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    let Some(f) = Assets::get("index.html") else { return (StatusCode::NOT_FOUND, "UI not built: run `npm run build` in ui/").into_response() };
    let mut resp =
        (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], f.data.into_owned()).into_response();
    if let (Some(tok), Some(t)) = (&app.token, q.get("t"))
        && **tok == **t
    {
        resp.headers_mut().insert(header::SET_COOKIE, format!("vt={tok}; HttpOnly; SameSite=Strict; Path=/").parse().unwrap());
    }
    resp
}

async fn asset(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    match Assets::get(&path) {
        Some(f) => (StatusCode::OK, [(header::CONTENT_TYPE, mime_for(&path)), (header::CACHE_CONTROL, "no-cache")], f.data.into_owned()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn mime_for(p: &str) -> &'static str {
    match p.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff2" => "font/woff2",
        "md" | "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn boot(State(app): State<App>) -> ApiResult {
    // First frame: workspace + last note straight from disk, before the index is ready.
    blocking(&app, |e| {
        let ws = e.workspace();
        let first = ws.get("last").and_then(|v| v.as_str()).and_then(|p| e.read_note(p).ok());
        let o = e.obsidian();
        let notes = json!({"daily_folder": o.daily_folder, "daily_format": o.daily_format, "daily_template": o.daily_template, "templates_folder": o.templates_folder});
        Ok(json!({"workspace": ws, "first": first, "status": e.status(), "vault": e.root().file_name().and_then(|s| s.to_str()).unwrap_or("Vault"), "notes": notes}))
    })
    .await
}

fn vault_json(p: &std::path::Path, current: bool) -> Value {
    json!({"path": p, "name": p.file_name().and_then(|s| s.to_str()).unwrap_or("Vault"), "current": current, "missing": !p.is_dir()})
}

/// The open vault and the recent ones the app remembers. `switchable` is false under `vault serve`.
async fn vaults_get(State(app): State<App>) -> ApiResult {
    let host = app.host.clone();
    blocking(&app, move |e| {
        let here = e.root().canonicalize().unwrap_or_else(|_| e.root().to_path_buf());
        let mut list = vec![vault_json(&here, true)];
        for p in host.as_ref().map(|h| h.recent()).unwrap_or_default() {
            if p.canonicalize().unwrap_or_else(|_| p.clone()) != here && !list.iter().any(|v| v["path"] == json!(p)) {
                list.push(vault_json(&p, false));
            }
        }
        Ok(json!({"switchable": host.is_some(), "vaults": list}))
    })
    .await
}

#[derive(Deserialize)]
struct OpenBody {
    /// The folder to open; absent = ask with the system folder picker.
    path: Option<PathBuf>,
}

/// Open another vault. The engine is opened here first, so a folder that can't be opened (gone, or
/// in use by another program) leaves the current vault running and comes back as an error.
async fn vaults_open(State(app): State<App>, Json(b): Json<OpenBody>) -> ApiResult {
    let Some(host) = app.host.clone() else {
        return Err(ApiError(CoreError::Invalid("switching vaults needs the Cēra app".into())));
    };
    blocking(&app, move |e| {
        let Some(path) = b.path.or_else(|| host.pick_folder()) else { return Ok(json!({"opened": false})) };
        if !path.is_dir() {
            return Err(CoreError::NotFound(format!("{} is not a folder", path.display())));
        }
        let path = path.canonicalize().unwrap_or(path);
        if e.root().canonicalize().ok().as_deref() == Some(path.as_path()) {
            return Ok(json!({"opened": false, "current": true}));
        }
        let next = Engine::open(&path, EngineOptions::default()).map_err(|err| CoreError::Other(format!("couldn't open {}: {err}", path.display())))?;
        let name = vault_json(&path, true)["name"].clone();
        host.switch_to(path, next);
        Ok(json!({"opened": true, "name": name}))
    })
    .await
}

async fn vaults_forget(State(app): State<App>, Json(b): Json<OpenBody>) -> ApiResult {
    let (Some(host), Some(path)) = (app.host.clone(), b.path) else { return Ok(Json(json!({"ok": false}))) };
    blocking(&app, move |_| {
        host.forget(&path);
        Ok(json!({"ok": true}))
    })
    .await
}

async fn status(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.status())).await
}

async fn dir(State(app): State<App>, Query(q): Query<std::collections::HashMap<String, String>>) -> ApiResult {
    let p = q.get("path").cloned().unwrap_or_default();
    blocking(&app, move |e| Ok(e.list_dir(&p))).await
}

async fn recent(State(app): State<App>, Query(q): Query<LimitQ>) -> ApiResult {
    blocking(&app, move |e| Ok(e.recent(q.limit.unwrap_or(30)))).await
}

async fn note_get(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| {
        let v = e.read_note(&q.path)?;
        e.touch(&v.path, 1.0);
        Ok(v)
    })
    .await
}

#[derive(Deserialize)]
struct SaveBody {
    path: String,
    content: String,
    base: Option<String>,
}

async fn note_put(State(app): State<App>, Json(b): Json<SaveBody>) -> ApiResult {
    blocking(&app, move |e| e.save_note(&b.path, &b.content, b.base.as_deref())).await
}

#[derive(Deserialize)]
struct CreateBody {
    folder: Option<String>,
    title: String,
    content: Option<String>,
}

async fn note_create(State(app): State<App>, Json(b): Json<CreateBody>) -> ApiResult {
    blocking(&app, move |e| e.create_note(b.folder.as_deref().unwrap_or(""), &b.title, b.content.as_deref().unwrap_or(""))).await
}

#[derive(Deserialize)]
struct FromTo {
    from: String,
    to: String,
}

async fn rename(State(app): State<App>, Json(b): Json<FromTo>) -> ApiResult {
    blocking(&app, move |e| e.rename(&b.from, &b.to).map(|p| json!({"path": p}))).await
}

async fn delete(State(app): State<App>, Json(b): Json<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.delete(&b.path).map(|_| json!({"ok": true}))).await
}

async fn folder_create(State(app): State<App>, Json(b): Json<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.create_folder(&b.path).map(|p| json!({"path": p}))).await
}

async fn folder_rename(State(app): State<App>, Json(b): Json<FromTo>) -> ApiResult {
    blocking(&app, move |e| e.rename_folder(&b.from, &b.to).map(|p| json!({"path": p}))).await
}

async fn folder_delete(State(app): State<App>, Json(b): Json<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.delete_folder(&b.path).map(|_| json!({"ok": true}))).await
}

async fn switch(State(app): State<App>, Query(q): Query<LimitQ>) -> ApiResult {
    blocking(&app, move |e| Ok(e.switch(q.q.as_deref().unwrap_or(""), q.limit.unwrap_or(20)))).await
}

async fn search(State(app): State<App>, Query(q): Query<LimitQ>) -> ApiResult {
    blocking(&app, move |e| {
        let qs = q.q.unwrap_or_default();
        let t = std::time::Instant::now();
        let r = if q.mode.as_deref() == Some("text") { e.search_text(&qs, q.limit.unwrap_or(30))? } else { e.search(&qs, q.limit.unwrap_or(30))? };
        Ok(json!({"results": r, "ms": t.elapsed().as_secs_f64() * 1000.0}))
    })
    .await
}

#[derive(Deserialize)]
struct ResolveQ {
    target: String,
    from: Option<String>,
}

async fn resolve(State(app): State<App>, Query(q): Query<ResolveQ>) -> ApiResult {
    blocking(&app, move |e| Ok(json!({"path": e.resolve(&q.target, q.from.as_deref().unwrap_or(""))}))).await
}

async fn backlinks(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.backlinks(&q.path)).await
}

async fn mentions(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.unlinked_mentions(&q.path, 50)).await
}

async fn outgoing(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.outgoing_mentions(&q.path)).await
}

#[derive(Deserialize)]
struct LinkMention {
    src: String,
    start: u32,
    end: u32,
    target: String,
}

async fn link_mention(State(app): State<App>, Json(b): Json<LinkMention>) -> ApiResult {
    blocking(&app, move |e| e.link_mention(&b.src, b.start, b.end, &b.target)).await
}

async fn related(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| e.related(&q.path, 8)).await
}

async fn suggest(State(app): State<App>, Query(q): Query<PathQ>) -> ApiResult {
    blocking(&app, move |e| {
        Ok(json!({
            "links": e.link_suggestions(&q.path, 6)?,
            "tags": e.tag_suggestions(&q.path, 4)?.into_iter().map(|(t, s)| json!({"tag": t, "score": s})).collect::<Vec<_>>(),
            "dupes": e.near_duplicates(&q.path).into_iter().map(|(n, j)| json!({"path": n.path, "title": n.title, "jaccard": j})).collect::<Vec<_>>(),
        }))
    })
    .await
}

async fn dupes(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.all_near_duplicates())).await
}

async fn tags(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.tags())).await
}

#[derive(Deserialize)]
struct TagQ {
    tag: String,
}

async fn tag(State(app): State<App>, Query(q): Query<TagQ>) -> ApiResult {
    blocking(&app, move |e| Ok(e.notes_with_tag(&q.tag))).await
}

async fn clusters(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.clusters(2))).await
}

#[derive(Deserialize)]
struct GraphQ {
    path: Option<String>,
    depth: Option<usize>,
    max: Option<usize>,
}

async fn graph(State(app): State<App>, Query(q): Query<GraphQ>) -> ApiResult {
    blocking(&app, move |e| e.graph(q.path.as_deref().filter(|p| !p.is_empty()), q.depth.unwrap_or(2), q.max.unwrap_or(20_000))).await
}

async fn stats(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.stats())).await
}

async fn ws_get(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| Ok(e.workspace())).await
}

async fn ws_put(State(app): State<App>, Json(v): Json<Value>) -> ApiResult {
    blocking(&app, move |e| e.set_workspace(&v).map(|_| json!({"ok": true}))).await
}

#[derive(Deserialize)]
struct AttachQ {
    note: String,
    name: String,
}

async fn attachment(State(app): State<App>, Query(q): Query<AttachQ>, body: axum::body::Bytes) -> ApiResult {
    if body.len() > 200 << 20 {
        return Err(ApiError(CoreError::Invalid("attachment larger than 200 MB".into())));
    }
    blocking(&app, move |e| e.save_attachment(&q.note, &q.name, &body).map(|p| json!({"path": p}))).await
}

async fn rescan(State(app): State<App>) -> ApiResult {
    blocking(&app, |e| {
        e.rescan();
        Ok(json!({"ok": true}))
    })
    .await
}

async fn recall(State(app): State<App>, Query(q): Query<LimitQ>) -> ApiResult {
    let query = q.q.unwrap_or_default();
    let opts = vault_core::RecallOptions { limit: q.limit.unwrap_or(8).clamp(1, 20), ..Default::default() };
    blocking(&app, move |e| e.recall(&query, &opts)).await
}

/// Server-sent events: `sources` (the recall), `model`, `delta`…, then `done` or `error`.
async fn ask(State(app): State<App>, Json(req): Json<ai::AskRequest>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(ai::run_ask(app.engine.clone(), app.ai.clone(), req, tx));
    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn ai_get(State(app): State<App>) -> Json<Value> {
    Json(app.ai.public())
}

async fn ai_put(State(app): State<App>, Json(patch): Json<Value>) -> ApiResult {
    let ai = app.ai.clone();
    match tokio::task::spawn_blocking(move || ai.update(&patch)).await {
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(m)) => Err(ApiError(CoreError::Invalid(m))),
        Err(e) => Err(ApiError(CoreError::Other(e.to_string()))),
    }
}

#[derive(Deserialize)]
struct UrlQ {
    url: Option<String>,
    /// `api` for the hosted API (with the saved key); otherwise the local server.
    kind: Option<String>,
}

/// Models a local server offers, or with `kind=api` the hosted API offers the saved key.
/// `reachable: false` when nothing answers (or the key is refused, with `error` saying which).
async fn ai_models(State(app): State<App>, Query(q): Query<UrlQ>) -> Json<Value> {
    if q.kind.as_deref() == Some("api") {
        let url = q.url.filter(|u| !u.trim().is_empty());
        return Json(match app.ai.api_models(url.as_deref()).await {
            Ok(models) => json!({"reachable": true, "models": models}),
            Err(e) => json!({"reachable": false, "models": [], "error": e}),
        });
    }
    let url = q.url.filter(|u| !u.trim().is_empty()).unwrap_or_else(|| app.ai.public()["local_url"].as_str().unwrap_or(ai::DEFAULT_LOCAL_URL).to_string());
    let models = app.ai.local_models(url.trim_end_matches('/')).await;
    Json(json!({"url": url, "reachable": models.is_some(), "models": models.unwrap_or_default()}))
}

async fn events(State(app): State<App>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    use futures_util::StreamExt;
    let rx = app.engine.subscribe();
    let closing = app.closing.clone();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(|m| async move { m.ok().map(|ev| Ok(Event::default().event("change").json_data(ev).unwrap_or_default())) })
        .take_until(async move {
            match closing {
                Some(mut c) => drop(c.wait_for(|c| *c).await),
                None => std::future::pending().await,
            }
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn file(State(app): State<App>, axum::extract::Path(path): axum::extract::Path<String>, headers: HeaderMap) -> Response {
    let _ = headers;
    let root = app.engine.root().to_path_buf();
    if path.split('/').any(|s| s == ".." || s.starts_with('.')) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let p = root.join(&path);
    match tokio::fs::read(&p).await {
        Ok(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, mime_for(&path)), (header::CACHE_CONTROL, "no-cache")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// MCP over Streamable HTTP, answering every request with a single JSON response. Writes are
/// offered only when the client sends `x-vault-mcp-write: 1`.
async fn mcp_post(State(app): State<App>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    // DNS-rebinding guard: a browser page from another site may not talk to the endpoint.
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && !["http://127.0.0.1", "http://localhost", "http://[::1]"].iter().any(|o| origin == *o || origin.starts_with(&format!("{o}:")))
    {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})).into_response(),
    };
    let write = headers.get("x-vault-mcp-write").and_then(|v| v.to_str().ok()) == Some("1");
    let m = mcp::Mcp::new(app.engine.clone(), write);
    match tokio::task::spawn_blocking(move || m.handle(&msg)).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
