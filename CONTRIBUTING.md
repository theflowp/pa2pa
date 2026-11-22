# Contributing to pa2pa

Thanks for your interest in contributing to **pa2pa** (Personal Agent to Personal Agent)!

## Prerequisites

- **Rust** (stable toolchain via [rustup](https://rustup.rs))
- **Linux only (for `pa2pa-pill`)**:
  ```bash
  sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev
  ```

## Workspace Structure

```text
crates/
├── pa2pa-crypto/   # OpenSSH Ed25519, Ed25519->X25519, XChaCha20-Poly1305 E2E & at-rest crypto
├── pa2pa-proto/    # Message schema, attachments, invite URIs, WebSocket relay frames
├── pa2pa-core/     # Config, Markdown+YAML contacts, encrypted SQLite store, policy engine
├── pa2pa-relay/    # Zero-knowledge WebSocket relay server (rate limiting, allowlists, presence)
├── pa2pa-mcp/      # Built-in stdio MCP server (tools + resources)
├── pa2pa-daemon/   # Persistent relay client, 8 agent runners, OS sandbox, Web UI & SSE API
├── pa2pa-cli/      # Unified `pa2pa` binary (CLI, MCP, daemon, relay, service, setup-mcp)
└── pa2pa-pill/     # Cross-platform menu bar / system tray icon + floating desktop pill
```

## Running Tests & Local Demo

```bash
# Run all unit + end-to-end integration tests
cargo test --workspace

# Build both binaries
cargo build --bin pa2pa --bin pa2pa-pill

# Run the isolated Flo <-> Benjamin local E2E simulation
./scripts/demo-local.sh
```

## Pull Request Guidelines

- Keep changes focused and well-tested (add unit or E2E tests for protocol, crypto, or security changes).
- Never bypass SSH signature checks, local daemon `Host`/`Origin`/`X-Pa2pa-Token` checks, or anti-prompt-injection boundaries.
- Ensure `cargo test --workspace` passes with zero warnings before opening a PR.
