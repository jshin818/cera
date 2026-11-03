//! vault-server: a localhost HTTP shell over vault-core. It serves the UI (embedded from
//! `ui/dist`) and a JSON API. The Tauri app and `vault serve` both use it.
//!
//! Every engine call runs on tokio's blocking pool, so request handling never stalls on I/O.
//! When a token is set, `/api/*` and `/file/*` require it (cookie set by `/?t=<token>`, or the
//! `x-vault-token` header), so other local processes can't read the vault through the port.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use vault_core::{CoreError, Engine, EngineOptions};

#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist"]
struct Assets;

pub struct ServeOptions {
    pub vault: PathBuf,
    pub addr: String,
    pub open_browser: bool,
    /// Require this token on API calls. `None` = no auth (still bound to localhost).
    pub token: Option<String>,
    /// Called with the bound address and the URL to open once the server is listening.
    pub on_ready: Option<Box<dyn FnOnce(SocketAddr, String) + Send>>,
}

#[derive(Clone)]
struct App {
    engine: Engine,
    token: Option<Arc<str>>,
}

pub fn random_token() -> String {
    use rand::Rng;
    let mut r = rand::rng();
    (0..32).map(|_| format!("{:x}", r.random_range(0..16u8))).collect()
}

/// Open the vault and serve until Ctrl-C.
pub async fn serve(o: ServeOptions) -> anyhow::Result<()> {
    let vault = o.vault.clone();
    let engine = tokio::task::spawn_blocking(move || Engine::open(&vault, EngineOptions::default())).await??;
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
    let app = router(engine.clone(), o.token);
    let e2 = engine.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("Vault: shutting down");
        })
        .await?;
    tokio::task::spawn_blocking(move || e2.shutdown()).await?;
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
    let state = App { engine, token: token.map(Arc::from) };
    let api = Router::new()
        .route("/api/boot", get(boot))
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
        .layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .merge(api)
        .route("/", get(index))
        .route("/{*path}", get(asset))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state)
}

async fn auth(State(app): State<App>, req: Request<Body>, next: Next) -> Response {
    if let Some(tok) = &app.token {
        let headers = req.headers();
        let ok = headers.get("x-vault-token").and_then(|v| v.to_str().ok()) == Some(tok)
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
    let mut resp = (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], f.data.into_owned()).into_response();
    if let (Some(tok), Some(t)) = (&app.token, q.get("t")) {
        if **tok == **t {
            resp.headers_mut().insert(header::SET_COOKIE, format!("vt={tok}; HttpOnly; SameSite=Strict; Path=/").parse().unwrap());
        }
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
        Ok(json!({"workspace": ws, "first": first, "status": e.status(), "vault": e.root().file_name().and_then(|s| s.to_str()).unwrap_or("Vault")}))
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

async fn events(State(app): State<App>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    use futures_util::StreamExt;
    let rx = app.engine.subscribe();
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|m| async move { m.ok().map(|ev| Ok(Event::default().event("change").json_data(ev).unwrap_or_default())) });
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
