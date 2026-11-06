//! The vault switcher endpoints, with a stand-in for the desktop app.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use vault_core::{Engine, EngineOptions};
use vault_server::{VaultHost, router, router_with_host};

#[derive(Default)]
struct FakeHost {
    recent: Mutex<Vec<PathBuf>>,
    pick: Mutex<Option<PathBuf>>,
    switched: Mutex<Vec<PathBuf>>,
}

impl VaultHost for FakeHost {
    fn recent(&self) -> Vec<PathBuf> {
        self.recent.lock().unwrap().clone()
    }
    fn forget(&self, vault: &Path) {
        self.recent.lock().unwrap().retain(|p| p != vault);
    }
    fn pick_folder(&self) -> Option<PathBuf> {
        self.pick.lock().unwrap().take()
    }
    fn switch_to(&self, vault: PathBuf, engine: Engine) {
        assert_eq!(engine.root(), vault);
        engine.shutdown();
        self.switched.lock().unwrap().push(vault);
    }
}

fn vault_dir(note: &str) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("Note.md"), note).unwrap();
    d
}

fn open(d: &Path) -> Engine {
    Engine::open(d, EngineOptions { watch: false, models_dir: None, ..Default::default() }).unwrap()
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn without_the_app_there_is_nothing_to_switch() {
    let a = vault_dir("a");
    let e = open(a.path());
    let app = router(e.clone(), None);
    let (s, v) = call(&app, "GET", "/api/vaults", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["switchable"], false);
    assert_eq!(v["vaults"].as_array().unwrap().len(), 1);
    assert_eq!(v["vaults"][0]["current"], true);
    let (s, _) = call(&app, "POST", "/api/vaults/open", Some(json!({"path": a.path()}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    e.shutdown();
}

#[tokio::test]
async fn lists_recent_and_switches() {
    let (a, b) = (vault_dir("a"), vault_dir("b"));
    let (a_path, b_path) = (a.path().canonicalize().unwrap(), b.path().canonicalize().unwrap());
    let gone = a_path.join("no-such-vault");
    let host = Arc::new(FakeHost::default());
    *host.recent.lock().unwrap() = vec![a_path.clone(), b_path.clone(), gone.clone()];
    let e = open(&a_path);
    let app = router_with_host(e.clone(), None, Some(host.clone()));

    // The open vault comes first and isn't repeated; a vanished folder is flagged.
    let (_, v) = call(&app, "GET", "/api/vaults", None).await;
    assert_eq!(v["switchable"], true);
    let list = v["vaults"].as_array().unwrap();
    assert_eq!(list.len(), 3, "{v}");
    assert_eq!(list[0]["path"], json!(a_path));
    assert_eq!(list[1]["name"], json!(b_path.file_name().unwrap().to_str().unwrap()));
    assert_eq!(list[2]["missing"], true);

    // The current vault and a missing folder don't switch.
    assert_eq!(call(&app, "POST", "/api/vaults/open", Some(json!({"path": a_path}))).await.1["opened"], false);
    assert_eq!(call(&app, "POST", "/api/vaults/open", Some(json!({"path": gone}))).await.0, StatusCode::NOT_FOUND);
    // Cancelling the folder picker is not an error.
    assert_eq!(call(&app, "POST", "/api/vaults/open", Some(json!({}))).await.1["opened"], false);
    assert!(host.switched.lock().unwrap().is_empty());

    let (s, v) = call(&app, "POST", "/api/vaults/open", Some(json!({"path": b_path}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["opened"], true);
    // And through the picker.
    *host.pick.lock().unwrap() = Some(b_path.clone());
    assert_eq!(call(&app, "POST", "/api/vaults/open", Some(json!({}))).await.1["opened"], true);
    assert_eq!(*host.switched.lock().unwrap(), [b_path.clone(), b_path.clone()]);

    call(&app, "POST", "/api/vaults/forget", Some(json!({"path": gone}))).await;
    assert_eq!(host.recent().len(), 2);
    e.shutdown();
}

#[tokio::test]
async fn a_vault_in_use_stays_put_and_reports_why() {
    let (a, b) = (vault_dir("a"), vault_dir("b"));
    let host = Arc::new(FakeHost::default());
    let e = open(a.path());
    let busy = open(b.path()); // another process would hold this lock
    let app = router_with_host(e.clone(), None, Some(host.clone()));
    let (s, v) = call(&app, "POST", "/api/vaults/open", Some(json!({"path": b.path()}))).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
    assert!(v["message"].as_str().unwrap().contains("already open"), "{v}");
    assert!(host.switched.lock().unwrap().is_empty());
    busy.shutdown();
    e.shutdown();
}
