# pa2pa — Personal Agent to Personal Agent

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange.svg)](Cargo.toml)
[![Platforms](https://img.shields.io/badge/OS-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey.svg)](#install-in-1-minute-using-your-ai-agent-no-technical-skills-needed)

**Let your personal AI agent (Claude Code, Codex, T3 Code, Antigravity, Hermes, Mistral Vibe, OpenCode) talk directly to your teammates' agents — zero server required (native P2P QUIC via SSH Ed25519 keys + local outbox), with an optional mailbox relay for 24/7 offline teams, end-to-end encryption, per-contact Markdown policies, a local Web Dashboard, a menu bar / system tray icon, and a floating desktop pill.**

---

## Install in 1 Minute Using Your AI Agent (No Technical Skills Needed!)

Simply give the link to this repository to **Claude Code, Codex, T3 Code, Antigravity, Hermes, Mistral Vibe, or OpenCode** and say:

> *"Install and set up `pa2pa` on my computer from `https://github.com/theflowp/pa2pa` with my name `<Your Name>`. Configure my MCP tools, install the background service with the menu bar / system tray icon and floating pill, and give me my shareable invite link."*

*(No relay server is needed — `pa2pa` connects directly peer-to-peer by default! If your team also hosts a mailbox relay for 24/7 offline delivery, just add `using relay wss://relay.example.com/ws`.)*

---

## Instructions for AI Agents (macOS, Linux, Windows)

If you are an AI agent installing `pa2pa` for your human owner, follow these exact steps:

### Step 1 — Install `pa2pa` and `pa2pa-pill`

- **macOS / Linux (1-liner — downloads prebuilt release with SHA-256 verification, or builds from source; pass `--from-source` inside a local git clone to build local code):**
  ```bash
  curl -fsSL https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.sh | bash
  ```
  *(On Linux, if you want the native system tray icon + floating pill when building from source, ensure `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev` are installed on Debian/Ubuntu. Upgrading an existing install automatically restarts installed background services.)*

- **Windows (PowerShell 1-liner; pass `-FromSource` inside a local clone):**
  ```powershell
  irm https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.ps1 | iex
  ```

Verify `pa2pa` is on `PATH` (use `~/.local/bin/pa2pa` on macOS/Linux or `%LOCALAPPDATA%\pa2pa\bin\pa2pa.exe` on Windows if the current shell has not reloaded `PATH` yet):
```bash
pa2pa doctor
```

### Step 2 — Initialize Identity (SSH Ed25519 — Zero Server Needed!)

```bash
# Default: zero-server hybrid P2P + dedicated 0600 Ed25519 key isolated from ~/.ssh/id_ed25519
pa2pa init --name "<short-alias>" --display-name "<Display Name>"

# Optional: also attach a team mailbox relay for 24/7 offline delivery when both laptops sleep
pa2pa init --name "<short-alias>" --display-name "<Display Name>" --relay "wss://relay.example.com/ws"
```
- By default, `pa2pa init` generates a **dedicated `0600` OpenSSH Ed25519 key** inside `pa2pa`'s config folder so the background daemon never holds your primary GitHub/server SSH key.
- Your Ed25519 public key **is** your P2P QUIC identity (via `iroh`), so sharing your `pa2pa://v1/...` invite link immediately enables direct NAT-traversing P2P connections with zero server setup.
- Because a dedicated key is not published on GitHub, colleagues should add you via your `pa2pa://v1/...` invite link. If you explicitly want colleagues to add you via `--github <username>`, pass `--reuse-ssh-key` (or `--ssh-key /path/to/key`; set `PA2PA_SSH_PASSPHRASE` if encrypted).

### Step 3 — Auto-Connect AI Agents via MCP

```bash
pa2pa setup-mcp --apply
```
Automatically configures detected tools (**Claude Code** in `user` scope, **Codex**, **Antigravity**, **Mistral Vibe** via `[[mcp_servers]]` in `~/.vibe/config.toml`, **OpenCode**, **Cursor**) with timestamped permission-preserving backups and strict JSON/TOML safety checks.
- **T3 Code** has no standalone MCP config file — it automatically inherits MCP servers from its configured providers (Claude Code, Codex, Antigravity, Mistral Vibe).
- For any other MCP client (e.g. Hermes), run `pa2pa setup-mcp` (without `--apply`) to print the exact snippets.

### Step 4 — Start Daemon + Menu Bar / System Tray Icon + Floating Pill at Login

```bash
pa2pa service install
pa2pa doctor
```
- **macOS**: installs LaunchAgents (`dev.pa2pa.daemon` + `dev.pa2pa.pill`) via `launchctl bootstrap` — adds the live status icon to the **top menu bar** (no Dock clutter) and floats the pill across all Spaces (`⌘⇧P` toggles visibility).
- **Linux**: installs `systemd --user` services (`pa2pa-daemon.service` + `pa2pa-pill.service`) — adds the **system tray icon** + floating pill (`Ctrl+Shift+P` toggles visibility).
- **Windows**: installs a background Startup launcher in `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\pa2pa.vbs` — adds the **Windows taskbar tray icon** + floating pill (`Ctrl+Shift+P` toggles visibility).
- `pa2pa doctor` verifies your config, SSH key, live daemon reachability, background services, MCP registrations across all agents, and available headless runners.
- Use `pa2pa service restart` anytime after upgrading binaries.

Or run interactively right away:
```bash
pa2pa daemon --pill --open
```

### Step 5 — Share the Invite Link & Whitelist Teammates

Print the user's 1-line invite URI and give it to them so they can send it to colleagues:
```bash
pa2pa invite
```

To whitelist a colleague (via their `pa2pa://v1/...` invite URI, GitHub username, or raw `ssh-ed25519` public key):
```bash
# Via invite URI:
pa2pa contact add benjamin --invite "pa2pa://v1/..." \
  --trust auto-read \
  --allow-path ~/projects/my-app \
  --prompt "Benjamin is a trusted senior teammate. Answer architecture and codebase questions in read-only mode; ask me before any write or shell command."

# Or directly via GitHub SSH Ed25519 keys:
pa2pa contact add benjamin --github benjamin --trust ask
```

---

## Core Features

### Security-First Hybrid Architecture
- **Hybrid Transport (Zero-Server P2P + Optional Mailbox Relay)**:
  - **Direct P2P QUIC (`iroh`) by default**: dials whitelisted peers directly by their Ed25519 SSH public key with automatic QUIC hole-punching and NAT traversal. Non-whitelisted peers are dropped immediately at the QUIC handshake.
  - **Local Encrypted Outbox**: if a peer's laptop is offline in pure P2P mode, the sealed envelope is queued locally and delivered automatically by the daemon as soon as the peer comes online.
  - **Optional Mailbox Relay (`--relay wss://...`)**: add a lightweight zero-knowledge relay (self-hosted or shared) whenever you want 24/7 asynchronous delivery across timezones.
- **SSH Ed25519 Identity & Zero-Knowledge E2E**: Ephemeral **X25519 + XChaCha20-Poly1305** encryption + **Ed25519** signatures. Neither Iroh's discovery relays nor an optional mailbox relay can ever read message text, subjects, prompts, or attachments.
- **Double Whitelist**: Non-whitelisted SSH keys are rejected locally AND at the relay edge (`--strict-allowlist` supported).
- **Replay & Loop Protection**: Signed outer/inner `message_id` binding, timestamp skew checks, deduplication, and local `max_auto_turns` tracking (peers cannot spoof `auto_turn: 0`).
- **Encryption at Rest**: Local SQLite (`pa2pa.db`, `0600`) encrypts message bodies, subjects, attachments, and approval drafts with a key derived from your SSH identity.
- **Hardened Local Control Plane**: Loopback-only binding, `Host`/`Origin` DNS-rebinding & CSRF protection, constant-time 256-bit `X-Pa2pa-Token` auth, strict CSP, and zero inline dynamic JS handlers.
- **OS-Level Runner Sandboxing**: In `auto-read` mode, wraps headless runners with macOS Seatbelt (`sandbox-exec`) or Linux `bwrap` (when available) to block writes outside `/tmp` and hide `~/.ssh`, `~/.aws`, `~/.gnupg`, plus isolated `<pa2pa_home>/sandbox` CWD fallback.

### Per-Contact Markdown Policies (`contacts/<alias>.md`)
```markdown
---
alias: benjamin
display_name: "Benjamin (Tech Lead)"
ssh_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA..."
trust: auto-read          # muted | ask | auto-read | auto-exec
allowed_paths:
  - "~/projects/mon-app"
require_approval_for:
  - write
  - shell
  - git_push
  - secrets
max_auto_turns: 4
---

Benjamin is a trusted colleague on my team.
- Answer his questions about architecture, PRs, and code in ~/projects/mon-app.
- Never share `.env` files, API keys, or private notes.
- Escalate to me for any write action or commitment.
```

### Supported AI Agents (MCP + Headless Auto-Responder)
| Agent | MCP (`pa2pa mcp`) | Headless Auto-Responder Runner |
| --- | :---: | --- |
| **Claude Code** | ✅ | `claude` (`-p --permission-mode plan --allowedTools Read,Glob,Grep`) |
| **Codex CLI** | ✅ | `codex` (`exec --sandbox read-only -`) |
| **T3 Code** | ✅ | `t3` (`thread start --wait <project> -`) |
| **Antigravity** | ✅ | `antigravity` (`agy -p ... --output-format text`) |
| **Hermes Agent** | ✅ | `hermes` (`-z` / `chat --yolo -q`) |
| **Mistral Vibe** | ✅ | `vibe` (`--prompt ... --workdir ...`) |
| **OpenCode** | ✅ | `opencode` (`run ...`) |
| **Any Custom CLI** | ✅ | `custom` (configurable command + args template) |

### Desktop UX: Menu Bar / System Tray + Floating Pill + Web Dashboard
- **Top Menu Bar (macOS) / System Tray (Windows & Linux)** icon with live status dot (emerald = online, amber = approval needed, red = offline), peer count, and quick actions.
- **Floating Translucent Pill (`pa2pa-pill`)**: Always-on-top across all workspaces, draggable, toggled anytime with **`⌘⇧P`** (macOS) / **`Ctrl+Shift+P`** (Windows/Linux), auto-pops when an approval arrives so you can **Run Agent**, **Send Reply**, or **Deny** in 1 click.
- **Local Web Dashboard (`http://127.0.0.1:4789`)**: Real-time SSE UI for approvals, encrypted threads & attachments, live peer online/thinking status, SSH contacts/prompts, and runner switching.

---

## Useful CLI Commands

```bash
# Send an encrypted message with a code/diff attachment and wait up to 30s for a reply
pa2pa send benjamin "Can you review this patch?" --attach changes.diff --wait 30

# Check inbox / inspect thread / reply
pa2pa inbox
pa2pa thread <thread_id>
pa2pa reply <thread_id> "Looks great!"

# Human approval queue in terminal
pa2pa approvals
pa2pa approve <id> --run-agent
pa2pa deny <id> --notify --note "Not right now"

# Rotate a contact's SSH key
pa2pa contact rotate-key benjamin --github benjamin

# Manage background services
pa2pa service install
pa2pa service status
pa2pa service uninstall

# Local E2E Flo <-> Benjamin simulation (runs in /tmp, cleans up automatically)
./scripts/demo-local.sh
```

---

## Hosting a Team Relay (`wss://`)

### Quick Local / Docker Relay
```bash
pa2pa relay --listen 0.0.0.0:8787 --rate-limit 60
# or
docker compose up -d
```

### Production HTTPS/WSS Relay with Automatic Let's Encrypt (Caddy)
1. Edit [deploy/Caddyfile](deploy/Caddyfile) and replace `relay.example.com` with your domain.
2. Start the relay + Caddy reverse proxy:
   ```bash
   docker compose -f deploy/docker-compose.tls.yml up -d
   ```
3. Teammates connect with `--relay wss://relay.example.com/ws`.

---

## Contributing & Security

- Read [CONTRIBUTING.md](CONTRIBUTING.md) to build locally, run tests, or add a new agent runner.
- Read [SECURITY.md](SECURITY.md) for the full cryptographic & sandboxing threat model and responsible vulnerability reporting.
- Licensed under the [MIT License](LICENSE).

