//! Recall and Ask over HTTP against the sample vault: cited passages, AI settings, and the answer
//! stream when no model is set up (sources only).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use tower::ServiceExt;
use vault_core::{Engine, EngineOptions};

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

fn app() -> (tempfile::TempDir, tempfile::TempDir, axum::Router) {
    let d = tempfile::tempdir().unwrap();
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../vault-core/tests/fixtures/sample-vault"), d.path());
    let e = Engine::open(d.path(), EngineOptions { watch: false, models_dir: None, ..Default::default() }).unwrap();
    assert!(e.wait_ready(Duration::from_secs(20)));
    e.flush();
    let cfg = tempfile::tempdir().unwrap();
    let r = vault_server::router_with_ai(e, None, Some(cfg.path().join("ai.json")));
    (d, cfg, r)
}

async fn send(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// (event, data) pairs of a finished SSE body.
fn events(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter_map(|block| {
            let name = block.lines().find_map(|l| l.strip_prefix("event: "))?.to_string();
            let data = block.lines().find_map(|l| l.strip_prefix("data: "))?;
            Some((name, serde_json::from_str(data).unwrap_or(Value::Null)))
        })
        .collect()
}

#[tokio::test]
async fn recall_returns_cited_passages() {
    let (_d, _c, app) = app();
    let (code, body) = send(&app, "GET", "/api/recall?q=what%20did%20we%20decide%20to%20ship%3F", None).await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    let p = &v["passages"][0];
    assert_eq!(p["path"], "Projects/Beta.md", "{v}");
    assert_eq!(p["n"], 1);
    assert!(p["text"].as_str().unwrap().contains("We decided to ship"));
    assert!(p["line"].as_u64().unwrap() >= 1 && p["why"].is_array());
}

#[tokio::test]
async fn ai_settings_never_return_the_key() {
    let (_d, cfg, app) = app();
    let (code, body) = send(&app, "PUT", "/api/ai", Some(json!({"provider": "claude", "anthropic_api_key": "sk-ant-test"}))).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert!(!body.contains("sk-ant-test"));
    let (_, body) = send(&app, "GET", "/api/ai", None).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["has_key"], true);
    assert!(!body.contains("sk-ant-test"));
    // Kept outside the vault, in the config file.
    assert!(std::fs::read_to_string(cfg.path().join("ai.json")).unwrap().contains("sk-ant-test"));
    let (code, _) = send(&app, "PUT", "/api/ai", Some(json!({"local_url": "http://example.com:11434"}))).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ask_without_a_model_streams_sources_only() {
    let (_d, _c, app) = app();
    send(&app, "PUT", "/api/ai", Some(json!({"provider": "off"}))).await;
    let (code, body) = send(&app, "POST", "/api/ask", Some(json!({"question": "What did we decide about the beta?"}))).await;
    assert_eq!(code, StatusCode::OK);
    let ev = events(&body);
    assert_eq!(ev[0].0, "sources", "{body}");
    assert_eq!(ev[0].1["passages"][0]["path"], "Projects/Beta.md");
    let done = ev.iter().find(|(n, _)| n == "done").expect("done event");
    assert_eq!(done.1["answered"], false);
    assert!(!ev.iter().any(|(n, _)| n == "delta"));

    let (_, body) = send(&app, "POST", "/api/ask", Some(json!({"question": "  "}))).await;
    assert_eq!(events(&body)[0].0, "error");
}

/// A stand-in for Ollama / LM Studio: lists one model and streams a fixed answer, keeping the last request.
async fn fake_local_model() -> (String, std::sync::Arc<std::sync::Mutex<Value>>) {
    use axum::routing::{get, post};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Value::Null));
    let keep = seen.clone();
    let r = axum::Router::new().route("/v1/models", get(|| async { axum::Json(json!({"data": [{"id": "tiny"}]})) })).route(
        "/v1/chat/completions",
        post(move |axum::Json(body): axum::Json<Value>| {
            let keep = keep.clone();
            async move {
                *keep.lock().unwrap() = body;
                let chunks = ["We decided ", "to ship [1]."]
                    .iter()
                    .map(|t| format!("data: {}\n\n", json!({"choices": [{"delta": {"content": t}, "finish_reason": null}]})))
                    .chain(["data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()])
                    .collect::<String>();
                ([("content-type", "text/event-stream")], chunks)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    (url, seen)
}

#[tokio::test]
async fn ask_streams_a_cited_answer_from_a_local_model() {
    let (_d, _c, app) = app();
    let (url, seen) = fake_local_model().await;
    let (code, body) = send(&app, "PUT", "/api/ai", Some(json!({"provider": "local", "local_url": url}))).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (_, body) = send(&app, "GET", "/api/ai/models", None).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["models"], json!(["tiny"]));

    let (_, body) = send(&app, "POST", "/api/ask", Some(json!({"question": "What did we decide about the beta?"}))).await;
    let ev = events(&body);
    let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["sources", "model", "delta", "delta", "done"], "{body}");
    assert_eq!(ev[1].1["model"], "tiny");
    let answer: String = ev.iter().filter(|(n, _)| n == "delta").map(|(_, v)| v["text"].as_str().unwrap()).collect();
    assert_eq!(answer, "We decided to ship [1].");
    assert_eq!(ev[4].1["answered"], true);
    // The model saw the numbered sources and the citation rules.
    let req = seen.lock().unwrap().clone();
    assert_eq!(req["model"], "tiny");
    assert!(req["messages"][0]["content"].as_str().unwrap().contains("Cite every claim"));
    let user = req["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(user.contains("[1] Projects/Beta.md") && user.ends_with("Question: What did we decide about the beta?"), "{user}");
}
