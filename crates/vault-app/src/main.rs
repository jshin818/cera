//! Cēra desktop app. The window is a native WKWebView (Tauri 2) pointed at vault-server, which
//! runs in-process on 127.0.0.1 with a random port and a per-launch token. vault-core does all the
//! work on its own threads; the window only renders and sends commands.
//!
//! Vault selection: the first command-line argument, else the last vault opened, else a folder
//! picker. The choice is remembered in the app's config directory, with a list of recent vaults
//! for the switcher. Switching starts a server for the new vault, points the window at it and
//! stops the old one, all in this process.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use vault_server::{VaultEngine, VaultHost};

const MAX_RECENT: usize = 12;

fn config_file() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Cera"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("cera"))
    }?;
    Some(base.join("config.json"))
}

fn read_config() -> serde_json::Value {
    config_file().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
}

fn write_config(v: &serde_json::Value) {
    let Some(f) = config_file() else { return };
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let tmp = f.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
}

/// Recent vaults, newest first. Older configs only have `vault`.
fn recent_in(cfg: &serde_json::Value) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = cfg.get("recent").and_then(|r| r.as_array()).into_iter().flatten().filter_map(|p| p.as_str()).map(PathBuf::from).collect();
    if let Some(v) = cfg.get("vault").and_then(|v| v.as_str()).map(PathBuf::from)
        && !out.contains(&v)
    {
        out.insert(0, v);
    }
    out
}

/// `cfg` with `vault` opened last: it becomes the current vault and moves to the top of the list.
fn with_opened(cfg: &serde_json::Value, vault: &Path) -> serde_json::Value {
    let mut recent = recent_in(cfg);
    recent.retain(|p| p != vault);
    recent.insert(0, vault.to_path_buf());
    recent.truncate(MAX_RECENT);
    let mut cfg = cfg.clone();
    cfg["vault"] = serde_json::json!(vault);
    cfg["recent"] = serde_json::json!(recent);
    cfg
}

fn with_forgotten(cfg: &serde_json::Value, vault: &Path) -> serde_json::Value {
    let mut recent = recent_in(cfg);
    recent.retain(|p| p != vault);
    let mut cfg = cfg.clone();
    cfg["recent"] = serde_json::json!(recent);
    cfg
}

fn last_vault() -> Option<PathBuf> {
    let p = PathBuf::from(read_config().get("vault")?.as_str()?);
    p.is_dir().then_some(p)
}

fn remember(vault: &Path) {
    let vault = vault.canonicalize().unwrap_or_else(|_| vault.to_path_buf());
    write_config(&with_opened(&read_config(), &vault));
}

fn choose_vault() -> Option<PathBuf> {
    if let Some(arg) = std::env::args_os().nth(1).map(PathBuf::from).filter(|p| p.is_dir()) {
        return Some(arg);
    }
    last_vault().or_else(|| rfd::FileDialog::new().set_title("Choose a folder of Markdown notes (or an Obsidian vault)").pick_folder())
}

fn title_of(vault: &Path) -> String {
    vault.file_name().and_then(|s| s.to_str()).unwrap_or("Cera").to_string()
}

/// The switcher's side of the app: recent vaults, the folder picker, and handing a newly opened
/// vault to the server loop.
struct Host {
    handle: Arc<OnceLock<tauri::AppHandle>>,
    next: tokio::sync::mpsc::UnboundedSender<(PathBuf, VaultEngine)>,
}

impl VaultHost for Host {
    fn recent(&self) -> Vec<PathBuf> {
        recent_in(&read_config())
    }

    fn forget(&self, vault: &Path) {
        write_config(&with_forgotten(&read_config(), vault));
    }

    fn pick_folder(&self) -> Option<PathBuf> {
        // macOS wants native panels on the main thread; this caller is a server worker.
        let handle = self.handle.get()?;
        let (tx, rx) = mpsc::channel();
        handle
            .run_on_main_thread(move || {
                let _ = tx.send(rfd::FileDialog::new().set_title("Open a folder of Markdown notes as a vault").pick_folder());
            })
            .ok()?;
        rx.recv().ok().flatten()
    }

    fn switch_to(&self, vault: PathBuf, engine: VaultEngine) {
        remember(&vault);
        let _ = self.next.send((vault, engine));
    }
}

fn main() -> anyhow::Result<()> {
    let Some(vault) = choose_vault() else { return Ok(()) };
    remember(&vault);

    // Engine + localhost server on their own runtime; the first URL comes back once it is
    // listening, later ones (after a switch) go straight to the window.
    let (first_tx, first_rx) = mpsc::channel::<String>();
    let first_tx = Arc::new(Mutex::new(Some(first_tx)));
    let handle: Arc<OnceLock<tauri::AppHandle>> = Arc::default();
    let (next_tx, mut next_rx) = tokio::sync::mpsc::unbounded_channel();
    let host: Arc<dyn VaultHost> = Arc::new(Host { handle: handle.clone(), next: next_tx });
    let token = vault_server::random_token();
    let title = title_of(&vault);
    let handle2 = handle.clone();
    std::thread::Builder::new().name("vault-server".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        rt.block_on(async move {
            let mut pending = Some((vault, None));
            let mut stop_old: Option<tokio::sync::oneshot::Sender<()>> = None;
            loop {
                let (vault, engine) = match pending.take() {
                    Some(v) => v,
                    None => match next_rx.recv().await {
                        Some((v, e)) => (v, Some(e)),
                        None => break,
                    },
                };
                // The old server finishes in-flight requests and closes its engine once the
                // window has moved to the new one.
                if let Some(s) = stop_old.take() {
                    let _ = s.send(());
                }
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
                stop_old = Some(stop_tx);
                let first = engine.is_none();
                let (token2, vault2, handle3, first_tx) = (token.clone(), vault.clone(), handle2.clone(), first_tx.clone());
                let opts = vault_server::ServeOptions {
                    vault: vault.clone(),
                    engine,
                    addr: "127.0.0.1:0".into(),
                    open_browser: false,
                    token: Some(token.clone()),
                    on_ready: Some(Box::new(move |addr, url| {
                        // Lets `vault mcp` (AI agents) reach this vault through the running app.
                        if let Err(e) = vault_server::mcp::publish(&format!("http://{addr}"), &token2, &vault2) {
                            eprintln!("Cera: could not record the server for agents: {e}");
                        }
                        if let Some(tx) = first_tx.lock().unwrap().take() {
                            let _ = tx.send(url);
                        } else if let Some(w) = handle3.get().and_then(|h| tauri::Manager::get_webview_window(h, "main")) {
                            let _ = w.set_title(&title_of(&vault2));
                            if let Ok(u) = url.parse() {
                                let _ = w.navigate(u);
                            }
                        }
                    })),
                    host: Some(host.clone()),
                    shutdown: Some(Box::pin(async move {
                        let _ = stop_rx.await;
                    })),
                };
                tokio::spawn(async move {
                    if let Err(e) = vault_server::serve(opts).await {
                        eprintln!("Cera: engine failed for {}: {e:#}", vault.display());
                        if first {
                            std::process::exit(1);
                        }
                    }
                });
            }
        });
    })?;
    let url = first_rx.recv_timeout(std::time::Duration::from_secs(30))?;

    tauri::Builder::default()
        .setup(move |app| {
            let _ = handle.set(app.handle().clone());
            let window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url.parse()?))
                .title(&title)
                .inner_size(1280.0, 820.0)
                .min_inner_size(720.0, 480.0)
                // Tauri's native file-drop handler claims every drag (wry answers NSDragOperationCopy
                // without passing it to WebKit), so the page never gets dragover/drop and the
                // sidebar's HTML5 drag and drop silently does nothing. Cera doesn't take OS file
                // drops, so hand drags to the web view.
                .disable_drag_drop_handler()
                .build()?;
            let _ = window.set_focus();
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri: {e}"))?;
    vault_server::mcp::unpublish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recent_list_moves_opened_vault_to_top() {
        let old = json!({"vault": "/a"});
        assert_eq!(recent_in(&old), [PathBuf::from("/a")]);
        let c = with_opened(&old, Path::new("/b"));
        assert_eq!(c["vault"], "/b");
        assert_eq!(recent_in(&c), [PathBuf::from("/b"), PathBuf::from("/a")]);
        let c = with_opened(&c, Path::new("/a"));
        assert_eq!(recent_in(&c), [PathBuf::from("/a"), PathBuf::from("/b")]);
        let c = with_forgotten(&c, Path::new("/b"));
        assert_eq!(recent_in(&c), [PathBuf::from("/a")]);
    }

    #[test]
    fn recent_list_is_capped() {
        let mut c = json!({});
        for i in 0..20 {
            c = with_opened(&c, Path::new(&format!("/v{i}")));
        }
        let r = recent_in(&c);
        assert_eq!(r.len(), MAX_RECENT);
        assert_eq!(r[0], PathBuf::from("/v19"));
    }
}
