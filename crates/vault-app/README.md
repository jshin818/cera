# vault-app (Cēra desktop)

Tauri 2 shell over `vault-server`. Excluded from the workspace's default members so the engine
builds and tests on any machine without WebKit; build it explicitly on the Mac:

```bash
cargo install tauri-cli --version "^2" --locked   # once
cd crates/vault-app && cargo tauri build            # → target/release/bundle/macos/Cera.app
```

or `cargo bundle --release` from this folder (uses `[package.metadata.bundle]`). Unsigned builds
run locally; distribution needs an Apple Developer ID for signing and notarization.
