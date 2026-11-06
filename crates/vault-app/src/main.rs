//! Cēra desktop app. The window is a native WKWebView (Tauri 2) pointed at vault-server, which
//! runs in-process on 127.0.0.1 with a random port and a per-launch token. vault-core does all the
//! work on its own threads; the window only renders and sends commands.
//!
//! Vault selection: the first command-line argument, else the last vault opened, else a folder
//! picker. The choice is remembered in the app's config directory.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::mpsc;

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

fn last_vault() -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(config_file()?).ok()?).ok()?;
    let p = PathBuf::from(v.get("vault")?.as_str()?);
    p.is_dir().then_some(p)
}

fn remember(vault: &std::path::Path) {
    if let Some(f) = config_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, serde_json::json!({ "vault": vault }).to_string());
    }
}

fn choose_vault() -> Option<PathBuf> {
    if let Some(arg) = std::env::args_os().nth(1).map(PathBuf::from).filter(|p| p.is_dir()) {
        return Some(arg);
    }
    last_vault().or_else(|| rfd::FileDialog::new().set_title("Choose a folder of Markdown notes (or an Obsidian vault)").pick_folder())
}

fn main() -> anyhow::Result<()> {
    let Some(vault) = choose_vault() else { return Ok(()) };
    remember(&vault);

    // Engine + localhost server on their own runtime; the URL comes back once it is listening.
    let (tx, rx) = mpsc::channel::<String>();
    let token = vault_server::random_token();
    let vault2 = vault.clone();
    let (token2, vault3) = (token.clone(), vault.clone());
    std::thread::Builder::new().name("vault-server".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
        let res = rt.block_on(vault_server::serve(vault_server::ServeOptions {
            vault: vault2,
            addr: "127.0.0.1:0".into(),
            open_browser: false,
            token: Some(token),
            on_ready: Some(Box::new(move |addr, url| {
                // Lets `vault mcp` (AI agents) reach this vault through the running app.
                if let Err(e) = vault_server::mcp::publish(&format!("http://{addr}"), &token2, &vault3) {
                    eprintln!("Cera: could not record the server for agents: {e}");
                }
                let _ = tx.send(url);
            })),
        }));
        if let Err(e) = res {
            eprintln!("Cera: engine failed: {e:#}");
            std::process::exit(1);
        }
    })?;
    let url = rx.recv_timeout(std::time::Duration::from_secs(30))?;

    let title = vault.file_name().and_then(|s| s.to_str()).unwrap_or("Cera").to_string();
    tauri::Builder::default()
        .setup(move |app| {
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
