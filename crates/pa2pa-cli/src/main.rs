mod service;
mod setup_mcp;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use pa2pa_core::{
    build_safe_agent_prompt, deliver_message, fetch_github_ed25519_keys, load_attachment_from_path,
    local_endpoint_id, pull_pending_messages, Config, Contact, ContactStore, DaemonConfig,
    ExecutionMode, Pa2paPaths, RunnerKind, Store, TransportMode, TrustLevel,
};
use pa2pa_crypto::{Identity, PeerPublicKey};
use pa2pa_daemon::runners::{detect_installed_runners, run_agent_responder};
use pa2pa_proto::{AgentMessage, InvitePayload, MessageKind};
use pa2pa_relay::RelayConfig;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "pa2pa",
    version,
    about = "Personal Agent to Personal Agent — SSH-authenticated, E2E encrypted agent messaging"
)]
struct Cli {
    /// Custom pa2pa home directory (great for running multiple profiles locally)
    #[arg(long, global = true, env = "PA2PA_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize your local pa2pa identity, SSH key, and relay configuration
    Init {
        /// Your short alias (e.g. flo, benjamin)
        #[arg(long)]
        name: Option<String>,
        /// Human-friendly display name
        #[arg(long)]
        display_name: Option<String>,
        /// Optional mailbox relay WebSocket URL (omit for zero-server P2P mode!)
        #[arg(long)]
        relay: Option<String>,
        /// Transport mode: hybrid (default: P2P direct + optional relay), p2p-only, relay-only
        #[arg(long, default_value = "hybrid")]
        transport: TransportMode,
        /// Explicit path to an existing Ed25519 OpenSSH private key
        #[arg(long)]
        ssh_key: Option<PathBuf>,
        /// Reuse your primary ~/.ssh/id_ed25519 key (allows colleagues to add you via `--github <user>`)
        #[arg(long)]
        reuse_ssh_key: bool,
        /// Generate a dedicated Ed25519 key in pa2pa home (now the default; kept for compatibility)
        #[arg(long)]
        dedicated_key: bool,
        /// Default headless agent runner (claude, codex, hermes, antigravity, t3, vibe, opencode, custom, none)
        #[arg(long, default_value = "claude")]
        runner: RunnerKind,
        /// Local Web Dashboard & API bind address
        #[arg(long, default_value = "127.0.0.1:4789")]
        ui_bind: String,
        /// Overwrite existing config if already initialized
        #[arg(long)]
        force: bool,
    },

    /// Print your SSH identity, fingerprint, and shareable invite URI
    Whoami,

    /// Print your 1-line `pa2pa://` invite URI to share with a colleague
    Invite,

    /// Manage SSH-whitelisted contacts and their owner policy prompts
    Contact {
        #[command(subcommand)]
        action: ContactCmd,
    },

    /// Send an end-to-end encrypted message to a whitelisted peer's agent
    Send {
        /// Contact alias
        to: String,
        /// Message body
        message: String,
        /// Optional subject
        #[arg(long)]
        subject: Option<String>,
        /// Existing thread ID to continue
        #[arg(long)]
        thread: Option<String>,
        /// Attach a file, code snippet, diff, or log (repeatable, total <= 512 KiB)
        #[arg(long = "attach")]
        attachments: Vec<PathBuf>,
        /// Wait up to N seconds for a synchronous reply
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },

    /// Sync from relay and display received messages
    Inbox {
        /// Include already-read messages
        #[arg(long)]
        all: bool,
        /// Skip pulling from relay (local SQLite only)
        #[arg(long)]
        no_sync: bool,
    },

    /// Read a conversation thread by ID
    Thread { thread_id: String },

    /// Reply in an existing conversation thread
    Reply {
        thread_id: String,
        message: String,
        /// Attach a file, code snippet, diff, or log (repeatable)
        #[arg(long = "attach")]
        attachments: Vec<PathBuf>,
    },

    /// List pending human approvals
    Approvals {
        /// Show all historical approvals too
        #[arg(long)]
        all: bool,
    },

    /// Approve a pending request and send a reply or invoke your local agent
    Approve {
        /// Approval ID
        id: String,
        /// Custom reply text to send
        #[arg(long)]
        reply: Option<String>,
        /// Run your configured local agent now to craft the response
        #[arg(long)]
        run_agent: bool,
        /// Allow scoped execution mode when using --run-agent (default: read-only)
        #[arg(long)]
        exec: bool,
    },

    /// Deny a pending request
    Deny {
        /// Approval ID
        id: String,
        /// Optional explanation note
        #[arg(long)]
        note: Option<String>,
        /// Send an encrypted rejection notice back to the peer
        #[arg(long)]
        notify: bool,
    },

    /// Start the local daemon (persistent relay WS, auto-responder, Web UI & SSE)
    Daemon {
        /// Override local Web UI bind address (e.g. 127.0.0.1:4789)
        #[arg(long)]
        ui_bind: Option<String>,
        /// Also launch the floating desktop pill controller (`pa2pa-pill`)
        #[arg(long)]
        pill: bool,
        /// Open the local Web Dashboard in your browser
        #[arg(long)]
        open: bool,
    },

    /// Launch the floating desktop pill + menu bar / system tray controller connected to your running daemon
    Pill {
        /// Override daemon URL (defaults to config's ui_bind)
        #[arg(long)]
        url: Option<String>,
        /// Start minimized in the menu bar / system tray until an approval arrives or hotkey is pressed
        #[arg(long)]
        start_hidden: bool,
    },

    /// Install, remove, or inspect the background startup service (macOS launchd, Linux systemd, Windows Startup)
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },

    /// Run the zero-knowledge E2E WebSocket relay server
    Relay {
        /// Listen address (host:port)
        #[arg(long, default_value = "0.0.0.0:8787", env = "PA2PA_RELAY_LISTEN")]
        listen: String,
        /// SQLite database path for offline encrypted mailbox storage
        #[arg(long, default_value = "pa2pa-relay.db", env = "PA2PA_RELAY_DB")]
        db: PathBuf,
        /// Offline message retention TTL in hours
        #[arg(long, default_value_t = 168, env = "PA2PA_RELAY_TTL_HOURS")]
        ttl_hours: i64,
        /// Reject messages to recipients who have not yet registered an allowlist
        #[arg(long, default_value_t = false, env = "PA2PA_RELAY_STRICT_ALLOWLIST")]
        strict_allowlist: bool,
        /// Max sends per minute per authenticated SSH identity (0 = unlimited)
        #[arg(long, default_value_t = 60, env = "PA2PA_RELAY_RATE_LIMIT")]
        rate_limit: u32,
    },

    /// Run the built-in MCP server over stdio (for Claude Code, Codex, T3, Hermes, Vibe, OpenCode, Antigravity)
    Mcp,

    /// Show or automatically apply MCP configuration for supported AI agents
    SetupMcp {
        /// Automatically write MCP configuration for detected tools (Claude Code, Codex, Antigravity, Mistral Vibe, OpenCode, Cursor)
        #[arg(long)]
        apply: bool,
        /// Target tool when using --apply (all, claude, codex, antigravity, vibe, opencode, cursor)
        #[arg(long, default_value = "all")]
        target: String,
    },

    /// Check local setup, SSH key, MCP registrations, background services, and installed agent CLIs
    Doctor,
}

#[derive(Subcommand, Debug)]
enum ServiceCmd {
    /// Install & start pa2pa daemon (+ menu bar/tray icon & floating pill) on login
    Install {
        /// Disable installing the menu bar / floating pill alongside the daemon
        #[arg(long)]
        no_pill: bool,
    },
    /// Restart installed pa2pa login services (useful after upgrading binaries)
    Restart,
    /// Stop and uninstall pa2pa login services
    Uninstall,
    /// Show status of installed background services
    Status,
}

#[derive(Subcommand, Debug)]
enum ContactCmd {
    /// Add or update a whitelisted contact
    Add {
        /// Friendly nickname/alias (optional when using --invite or --github)
        alias: Option<String>,
        /// Shareable `pa2pa://v1/...` invite URI
        #[arg(long)]
        invite: Option<String>,
        /// Import Ed25519 SSH key from GitHub (`https://github.com/<user>.keys`)
        #[arg(long)]
        github: Option<String>,
        /// Raw OpenSSH Ed25519 public key (`ssh-ed25519 AAAA...`)
        #[arg(long)]
        ssh_key: Option<String>,
        /// Display name
        #[arg(long)]
        display_name: Option<String>,
        /// Relay override for this contact
        #[arg(long)]
        relay: Option<String>,
        /// Trust tier: muted | ask | auto-read | auto-exec
        #[arg(long, default_value = "ask")]
        trust: TrustLevel,
        /// Allowed workspace path (repeatable)
        #[arg(long = "allow-path")]
        allowed_paths: Vec<String>,
        /// Owner description & policy prompt for this contact
        #[arg(long, default_value = "")]
        prompt: String,
        /// Per-contact runner override
        #[arg(long)]
        runner: Option<RunnerKind>,
    },
    /// List all whitelisted contacts
    List,
    /// Show full contact details and Markdown policy file path
    Show { alias: String },
    /// Change trust level for a contact
    Trust { alias: String, level: TrustLevel },
    /// Rotate a contact's whitelisted SSH public key
    RotateKey {
        alias: String,
        #[arg(long)]
        invite: Option<String>,
        #[arg(long)]
        github: Option<String>,
        #[arg(long)]
        ssh_key: Option<String>,
    },
    /// Remove a contact from the SSH whitelist
    Remove { alias: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Never log to stdout during `pa2pa mcp` (stdout is reserved for JSON-RPC).
    let is_mcp = matches!(cli.command, Commands::Mcp);
    if !is_mcp {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    }

    let paths = Pa2paPaths::resolve(cli.home)?;

    match cli.command {
        Commands::Init {
            name,
            display_name,
            relay,
            transport,
            ssh_key,
            reuse_ssh_key,
            dedicated_key,
            runner,
            ui_bind,
            force,
        } => {
            cmd_init(
                &paths,
                name,
                display_name,
                relay,
                transport,
                ssh_key,
                reuse_ssh_key,
                dedicated_key,
                runner,
                ui_bind,
                force,
            )?;
        }
        Commands::Whoami => cmd_whoami(&paths)?,
        Commands::Invite => cmd_invite(&paths)?,
        Commands::Contact { action } => cmd_contact(&paths, action).await?,
        Commands::Send {
            to,
            message,
            subject,
            thread,
            attachments,
            wait,
        } => cmd_send(&paths, &to, &message, subject, thread, attachments, wait).await?,
        Commands::Inbox { all, no_sync } => cmd_inbox(&paths, all, no_sync).await?,
        Commands::Thread { thread_id } => cmd_thread(&paths, &thread_id)?,
        Commands::Reply {
            thread_id,
            message,
            attachments,
        } => cmd_reply(&paths, &thread_id, &message, attachments).await?,
        Commands::Approvals { all } => cmd_approvals(&paths, all)?,
        Commands::Approve {
            id,
            reply,
            run_agent,
            exec,
        } => cmd_approve(&paths, &id, reply, run_agent, exec).await?,
        Commands::Deny { id, note, notify } => cmd_deny(&paths, &id, note, notify).await?,
        Commands::Daemon {
            ui_bind,
            pill,
            open: open_browser,
        } => {
            let cfg = Config::load(&paths)?;
            let bind = ui_bind.clone().unwrap_or_else(|| cfg.daemon.ui_bind.clone());
            let base_url = format!("http://{bind}");

            if pill {
                spawn_pill_process(&base_url, false);
            }
            if open_browser {
                let url_clone = base_url.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let _ = open::that(url_clone);
                });
            }
            pa2pa_daemon::run_daemon(paths, ui_bind).await?;
        }
        Commands::Pill { url, start_hidden } => {
            let base_url = if let Some(u) = url {
                u
            } else {
                let cfg = Config::load(&paths)?;
                format!("http://{}", cfg.daemon.ui_bind)
            };
            launch_pill_foreground(&base_url, start_hidden)?;
        }
        Commands::Service { action } => match action {
            ServiceCmd::Install { no_pill } => service::install_service(&paths, !no_pill)?,
            ServiceCmd::Restart => service::restart_service()?,
            ServiceCmd::Uninstall => service::uninstall_service()?,
            ServiceCmd::Status => service::service_status()?,
        },
        Commands::Relay {
            listen,
            db,
            ttl_hours,
            strict_allowlist,
            rate_limit,
        } => {
            let cfg = RelayConfig {
                listen,
                db_path: db,
                ttl_seconds: ttl_hours.max(1) * 3600,
                strict_allowlist,
                max_sends_per_minute: rate_limit,
                ..RelayConfig::default()
            };
            pa2pa_relay::run_relay(cfg).await?;
        }
        Commands::Mcp => {
            pa2pa_mcp::run_mcp_stdio(paths).await?;
        }
        Commands::SetupMcp { apply, target } => setup_mcp::run_setup_mcp(&paths, apply, &target)?,
        Commands::Doctor => cmd_doctor(&paths).await?,
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    paths: &Pa2paPaths,
    name: Option<String>,
    display_name: Option<String>,
    relay: Option<String>,
    transport: TransportMode,
    ssh_key: Option<PathBuf>,
    reuse_ssh_key: bool,
    dedicated_key: bool,
    runner: RunnerKind,
    ui_bind: String,
    force: bool,
) -> Result<()> {
    if paths.config_path().exists() && !force {
        bail!(
            "pa2pa is already initialized at {} (pass --force to reinitialize)",
            paths.config_path().display()
        );
    }

    paths.ensure_dirs()?;

    let alias = name
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "me".into())
        .to_ascii_lowercase();
    let display = display_name.unwrap_or_else(|| alias.clone());

    let mut used_dedicated_key = false;
    let (key_path, identity) = if let Some(explicit) = ssh_key {
        let id = Identity::load_openssh(&explicit)?;
        println!("Using explicit SSH Ed25519 key: {}", explicit.display());
        (explicit, id)
    } else if reuse_ssh_key && !dedicated_key {
        let default_ssh = dirs::home_dir()
            .ok_or_else(|| anyhow!("Cannot locate HOME directory for --reuse-ssh-key"))?
            .join(".ssh")
            .join("id_ed25519");
        if !default_ssh.exists() {
            bail!(
                "--reuse-ssh-key was requested, but {} does not exist. \
                 Pass --ssh-key <path> or omit --reuse-ssh-key to generate a dedicated pa2pa key.",
                default_ssh.display()
            );
        }
        let id = Identity::load_openssh(&default_ssh)?;
        println!("Reusing existing SSH Ed25519 key: {}", default_ssh.display());
        (default_ssh, id)
    } else {
        used_dedicated_key = true;
        let p = paths.dedicated_key_path();
        let id = Identity::generate(&format!("{alias}@pa2pa"))?;
        id.save_openssh(&p, &format!("{alias}@pa2pa"))?;
        println!(
            "Generated dedicated 0600 Ed25519 key at {} (keeps your ~/.ssh/id_ed25519 untouched and isolated from the background daemon).",
            p.display()
        );
        (p, id)
    };

    let cfg = Config {
        name: alias.clone(),
        display_name: display.clone(),
        relay_url: relay.clone(),
        transport: transport.clone(),
        key_path: key_path.clone(),
        daemon: DaemonConfig {
            ui_bind,
            default_runner: runner,
            ..DaemonConfig::default()
        },
    };
    cfg.save(paths)?;
    let _ = Store::open_encrypted(&paths.db_path(), identity.clone())?;

    let invite = InvitePayload {
        name: cfg.name.clone(),
        display_name: Some(cfg.display_name.clone()),
        ssh_key: identity.openssh_public_key().to_string(),
        relay: cfg.relay_url.clone(),
    }
    .encode()?;

    println!("Initialized pa2pa for `{}` ({})", cfg.name, cfg.display_name);
    println!("  Home        : {}", paths.home.display());
    println!("  SSH key     : {}", key_path.display());
    println!("  Fingerprint : {}", identity.fingerprint());
    if let Ok(ep_id) = local_endpoint_id(&identity) {
        println!("  P2P ID      : {ep_id}");
    }
    println!("  Transport   : {}", cfg.transport);
    println!(
        "  Relay       : {}",
        cfg.relay_url
            .as_deref()
            .unwrap_or("(none — zero-server P2P mode)")
    );
    println!("\nShare this 1-line invite URI with trusted colleagues:");
    println!("  {invite}");
    if used_dedicated_key {
        println!(
            "\nNote: Because pa2pa uses a dedicated key (not your GitHub SSH key), colleagues must add you\n\
             using the `pa2pa://v1/...` invite link above (not `--github <user>`). To reuse ~/.ssh/id_ed25519\n\
             instead so `--github <user>` works, reinitialize with `pa2pa init --force --reuse-ssh-key`."
        );
    }
    Ok(())
}

fn cmd_whoami(paths: &Pa2paPaths) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let invite = InvitePayload {
        name: cfg.name.clone(),
        display_name: Some(cfg.display_name.clone()),
        ssh_key: id.openssh_public_key().to_string(),
        relay: cfg.relay_url.clone(),
    }
    .encode()?;
    println!("Alias       : {}", cfg.name);
    println!("Display     : {}", cfg.display_name);
    println!("Fingerprint : {}", id.fingerprint());
    if let Ok(ep_id) = local_endpoint_id(&id) {
        println!("P2P ID      : {ep_id}");
    }
    println!("Transport   : {}", cfg.transport);
    println!(
        "Relay       : {}",
        cfg.relay_url
            .as_deref()
            .unwrap_or("(none — zero-server P2P)")
    );
    println!("Dashboard   : http://{}", cfg.daemon.ui_bind);
    println!("Runner      : {}", cfg.daemon.default_runner);
    println!("SSH PubKey  : {}", id.openssh_public_key());
    println!("Invite URI  : {invite}");
    Ok(())
}

fn cmd_invite(paths: &Pa2paPaths) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let invite = InvitePayload {
        name: cfg.name,
        display_name: Some(cfg.display_name),
        ssh_key: id.openssh_public_key().to_string(),
        relay: cfg.relay_url,
    }
    .encode()?;
    println!("{invite}");
    Ok(())
}

async fn cmd_contact(paths: &Pa2paPaths, action: ContactCmd) -> Result<()> {
    let cs = ContactStore::new(paths);
    match action {
        ContactCmd::Add {
            alias,
            invite,
            github,
            ssh_key,
            display_name,
            relay,
            trust,
            allowed_paths,
            prompt,
            runner,
        } => {
            let (final_alias, final_display, final_key, final_relay) =
                if let Some(uri) = invite {
                    let inv = InvitePayload::decode(&uri)?;
                    (
                        alias.unwrap_or(inv.name),
                        display_name.or(inv.display_name),
                        inv.ssh_key,
                        relay.or(inv.relay),
                    )
                } else if let Some(gh) = github {
                    let keys = fetch_github_ed25519_keys(&gh).await?;
                    println!(
                        "Fetched {} ssh-ed25519 key(s) from GitHub for @{gh} (using first).",
                        keys.len()
                    );
                    (
                        alias.unwrap_or_else(|| gh.clone()),
                        display_name.or_else(|| Some(format!("@{gh}"))),
                        keys[0].openssh_public_key().to_string(),
                        relay,
                    )
                } else {
                    let a = alias.ok_or_else(|| anyhow!("Provide <alias>"))?;
                    let k = ssh_key
                        .ok_or_else(|| anyhow!("Provide --invite, --github, or --ssh-key"))?;
                    (a, display_name, k, relay)
                };

            let mut contact = Contact::new(
                final_alias,
                final_key,
                final_display,
                final_relay,
                trust,
                prompt,
            )?;
            contact.allowed_paths = allowed_paths;
            contact.runner = runner;
            let file = cs.save(&contact)?;
            println!(
                "Whitelisted @{} [{}] ({})",
                contact.alias, contact.trust, contact.fingerprint
            );
            println!("  Policy file: {}", file.display());
        }
        ContactCmd::List => {
            let list = cs.list()?;
            if list.is_empty() {
                println!("No whitelisted contacts yet. Add one with `pa2pa contact add`.");
                return Ok(());
            }
            for c in list {
                println!(
                    "@{:<16} [{:<9}] {}  {}",
                    c.alias,
                    c.trust.as_str(),
                    c.fingerprint,
                    c.display_name.unwrap_or_default()
                );
            }
        }
        ContactCmd::Show { alias } => {
            let c = cs
                .get(&alias)?
                .ok_or_else(|| anyhow!("Contact `{alias}` not found"))?;
            let path = cs.file_path(&alias)?;
            println!("File: {}\n", path.display());
            println!("{}", c.to_markdown()?);
        }
        ContactCmd::Trust { alias, level } => {
            let mut c = cs
                .get(&alias)?
                .ok_or_else(|| anyhow!("Contact `{alias}` not found"))?;
            c.trust = level;
            cs.save(&c)?;
            println!("Updated @{} trust level -> {}", c.alias, c.trust);
        }
        ContactCmd::RotateKey {
            alias,
            invite,
            github,
            ssh_key,
        } => {
            let mut c = cs
                .get(&alias)?
                .ok_or_else(|| anyhow!("Contact `{alias}` not found"))?;
            let new_key = if let Some(uri) = invite {
                let inv = InvitePayload::decode(&uri)?;
                if c.relay.is_none() {
                    c.relay = inv.relay;
                }
                inv.ssh_key
            } else if let Some(gh) = github {
                let keys = fetch_github_ed25519_keys(&gh).await?;
                keys[0].openssh_public_key().to_string()
            } else if let Some(k) = ssh_key {
                k
            } else {
                bail!("Provide --invite, --github, or --ssh-key to rotate key");
            };
            let peer = PeerPublicKey::from_openssh(&new_key)?;
            let old_fp = c.fingerprint.clone();
            c.ssh_key = peer.openssh_public_key().to_string();
            c.fingerprint = peer.fingerprint().to_string();
            cs.save(&c)?;
            println!(
                "Rotated SSH key for @{}: {} -> {}",
                c.alias, old_fp, c.fingerprint
            );
        }
        ContactCmd::Remove { alias } => {
            if cs.delete(&alias)? {
                println!("Removed @{alias} from SSH whitelist.");
            } else {
                println!("Contact @{alias} was not found.");
            }
        }
    }
    Ok(())
}

async fn cmd_send(
    paths: &Pa2paPaths,
    to: &str,
    body: &str,
    subject: Option<String>,
    thread_id: Option<String>,
    attachment_paths: Vec<PathBuf>,
    wait: u64,
) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let contact = contacts
        .get(to)?
        .ok_or_else(|| anyhow!("Contact `{to}` is not whitelisted"))?;
    let store = Store::open_encrypted(&paths.db_path(), id.clone())?;

    let mut attachments = Vec::new();
    for p in attachment_paths {
        attachments.push(load_attachment_from_path(&p)?);
    }

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        Some("cli".into()),
        body,
        subject,
        thread_id,
        None,
        0,
    )
    .with_attachments(attachments);
    let outcome = deliver_message(&cfg, &id, &contact, msg, &store, None).await?;
    ping_local_daemon(paths, &cfg.daemon.ui_bind).await;
    println!(
        "Sent E2E message {} -> @{} (thread: {}, via: {})",
        outcome.message.message_id, contact.alias, outcome.message.thread_id, outcome.method
    );
    println!("  {}", outcome.detail);

    if wait > 0 {
        println!("Waiting up to {wait}s for a reply...");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
        let sent_at = outcome.message.created_at;
        while tokio::time::Instant::now() < deadline {
            if let Some(ref relay_url) = cfg.relay_url {
                if cfg.transport.relay_enabled() {
                    let _ = pull_pending_messages(
                        relay_url,
                        &id,
                        &contacts,
                        &store,
                        cfg.daemon.sync_allowlist_to_relay,
                    )
                    .await;
                }
            }
            let msgs = store.list_thread_messages(&outcome.message.thread_id)?;
            if let Some(reply) = msgs
                .into_iter()
                .rev()
                .find(|m| m.direction == "inbound" && m.created_at >= sent_at)
            {
                println!("\nReply from @{}:\n{}", reply.peer_alias, reply.body);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        println!("No reply arrived within {wait}s (thread stays open asynchronously).");
    }
    Ok(())
}

async fn cmd_inbox(paths: &Pa2paPaths, all: bool, no_sync: bool) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let store = Store::open_encrypted(&paths.db_path(), id.clone())?;

    if !no_sync && cfg.transport.relay_enabled() {
        if let Some(ref relay_url) = cfg.relay_url {
            match pull_pending_messages(
                relay_url,
                &id,
                &contacts,
                &store,
                cfg.daemon.sync_allowlist_to_relay,
            )
            .await
            {
                Ok(pulled) if !pulled.is_empty() => {
                    println!(
                        "Synced {} new encrypted message(s) from relay.\n",
                        pulled.len()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Warning: could not sync relay ({e}); showing local inbox.")
                }
            }
        }
    }

    let msgs = store.list_inbox(!all, 30)?;
    if msgs.is_empty() {
        println!("Inbox is empty.");
        return Ok(());
    }

    for m in msgs {
        println!(
            "[{}] @{} (thread {}) [{}]",
            m.created_at.format("%Y-%m-%d %H:%M"),
            m.peer_alias,
            m.thread_id,
            m.status
        );
        if let Some(subj) = m.subject {
            println!("  Subject: {subj}");
        }
        println!("  {}\n", m.body);
    }
    Ok(())
}

fn cmd_thread(paths: &Pa2paPaths, thread_id: &str) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let store = Store::open_encrypted(&paths.db_path(), id)?;
    let msgs = store.list_thread_messages(thread_id)?;
    if msgs.is_empty() {
        println!("Thread `{thread_id}` not found.");
        return Ok(());
    }
    store.mark_thread_read(thread_id)?;
    for m in msgs {
        let who = if m.direction == "outbound" {
            "you".to_string()
        } else {
            format!("@{}", m.peer_alias)
        };
        println!(
            "[{}] {} ({}):\n{}",
            m.created_at.format("%Y-%m-%d %H:%M:%S"),
            who,
            m.sender_agent.unwrap_or_else(|| "agent".into()),
            m.body
        );
        for att in &m.attachments {
            println!("  [attachment: {} ({:?})]", att.filename, att.kind);
        }
        println!();
    }
    Ok(())
}

async fn cmd_reply(
    paths: &Pa2paPaths,
    thread_id: &str,
    body: &str,
    attachment_paths: Vec<PathBuf>,
) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let store = Store::open_encrypted(&paths.db_path(), id.clone())?;

    let history = store.list_thread_messages(thread_id)?;
    let last = history
        .last()
        .ok_or_else(|| anyhow!("Thread `{thread_id}` not found"))?;
    let contact = contacts
        .get(&last.peer_alias)?
        .ok_or_else(|| anyhow!("Contact `{}` is not whitelisted", last.peer_alias))?;

    let mut attachments = Vec::new();
    for p in attachment_paths {
        attachments.push(load_attachment_from_path(&p)?);
    }

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        Some("cli".into()),
        body,
        None,
        Some(thread_id.to_string()),
        Some(last.id.clone()),
        0,
    )
    .with_attachments(attachments);
    let outcome = deliver_message(&cfg, &id, &contact, msg, &store, None).await?;
    store.mark_thread_read(thread_id)?;
    ping_local_daemon(paths, &cfg.daemon.ui_bind).await;
    println!(
        "Replied to @{} (thread {}, via: {})",
        contact.alias, thread_id, outcome.method
    );
    Ok(())
}

fn cmd_approvals(paths: &Pa2paPaths, all: bool) -> Result<()> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let store = Store::open_encrypted(&paths.db_path(), id)?;
    let status = if all { None } else { Some("pending") };
    let list = store.list_approvals(status)?;
    if list.is_empty() {
        println!("No pending approvals.");
        return Ok(());
    }
    for a in list {
        println!(
            "#{} [{}] from @{} (thread {})",
            a.id, a.status, a.peer_alias, a.thread_id
        );
        println!("  Reason : {}", a.reason);
        println!("  Message: {}", a.peer_message);
        if let Some(p) = a.proposed_reply {
            println!("  Draft  : {p}");
        }
        println!();
    }
    Ok(())
}

async fn cmd_approve(
    paths: &Pa2paPaths,
    id: &str,
    reply: Option<String>,
    run_agent: bool,
    exec: bool,
) -> Result<()> {
    let cfg = Config::load(paths)?;
    let identity = Identity::load_openssh(&cfg.key_path)?;
    let store = Store::open_encrypted(&paths.db_path(), identity.clone())?;
    let contacts = ContactStore::new(paths);

    let approval = store
        .get_approval(id)?
        .ok_or_else(|| anyhow!("Approval `{id}` not found"))?;
    let contact = contacts
        .get(&approval.peer_alias)?
        .ok_or_else(|| anyhow!("Contact `{}` not found", approval.peer_alias))?;

    let reply_text = if run_agent {
        let history = store.list_thread_messages(&approval.thread_id)?;
        let incoming = AgentMessage {
            message_id: approval.message_id.clone(),
            thread_id: approval.thread_id.clone(),
            in_reply_to: None,
            kind: MessageKind::Chat,
            subject: None,
            body: approval.peer_message.clone(),
            attachments: Vec::new(),
            sender_name: approval.peer_alias.clone(),
            sender_agent: None,
            auto_turn: 0,
            created_at: approval.created_at,
        };
        let mode = if exec {
            ExecutionMode::ScopedExec
        } else {
            ExecutionMode::ReadOnly
        };
        let prompt = build_safe_agent_prompt(&contact, &incoming, &history, &mode);
        println!("Running local agent ({})...", cfg.daemon.default_runner);
        run_agent_responder(paths, &cfg, &contact, &mode, &prompt, &approval.peer_alias).await?
    } else {
        reply
            .or(approval.proposed_reply.clone())
            .unwrap_or_else(|| "Approved by owner.".to_string())
    };

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        Some(if run_agent {
            cfg.daemon.default_runner.as_str().to_string()
        } else {
            "human-approved".into()
        }),
        reply_text.clone(),
        None,
        Some(approval.thread_id.clone()),
        Some(approval.message_id.clone()),
        0,
    );
    let outcome = deliver_message(&cfg, &identity, &contact, msg, &store, None).await?;
    store.decide_approval(id, "approved", None)?;
    store.update_message_status(&approval.message_id, "replied")?;
    ping_local_daemon(paths, &cfg.daemon.ui_bind).await;
    println!(
        "Approved #{id} and delivered reply to @{} (via {}):",
        contact.alias, outcome.method
    );
    println!("{reply_text}");
    Ok(())
}

async fn cmd_deny(
    paths: &Pa2paPaths,
    id: &str,
    note: Option<String>,
    notify: bool,
) -> Result<()> {
    let cfg = Config::load(paths)?;
    let identity = Identity::load_openssh(&cfg.key_path)?;
    let store = Store::open_encrypted(&paths.db_path(), identity.clone())?;
    let approval = store
        .get_approval(id)?
        .ok_or_else(|| anyhow!("Approval `{id}` not found"))?;
    if notify {
        if let Some(contact) = ContactStore::new(paths).get(&approval.peer_alias)? {
            let mut msg = AgentMessage::new_chat(
                cfg.name.clone(),
                Some("human-denied".into()),
                note.clone()
                    .unwrap_or_else(|| "Request declined by owner.".into()),
                None,
                Some(approval.thread_id.clone()),
                Some(approval.message_id.clone()),
                0,
            );
            msg.kind = MessageKind::Rejection;
            let _ = deliver_message(&cfg, &identity, &contact, msg, &store, None).await;
            ping_local_daemon(paths, &cfg.daemon.ui_bind).await;
        }
    }
    store.decide_approval(id, "denied", note.as_deref())?;
    store.update_message_status(&approval.message_id, "denied")?;
    println!("Denied approval #{id}.");
    Ok(())
}

async fn cmd_doctor(paths: &Pa2paPaths) -> Result<()> {
    println!("pa2pa doctor ({})\n", paths.home.display());
    match Config::load(paths) {
        Ok(cfg) => {
            println!("  [OK] Config loaded: @{} ({})", cfg.name, cfg.display_name);
            match Identity::load_openssh(&cfg.key_path) {
                Ok(id) => {
                    println!("  [OK] SSH Ed25519 key: {}", id.fingerprint());
                    if let Ok(ep_id) = local_endpoint_id(&id) {
                        println!("  [OK] P2P EndpointId : {ep_id}");
                    }
                }
                Err(e) => println!("  [ERR] SSH key invalid: {e}"),
            }
            println!("  [OK] Transport mode : {}", cfg.transport);
            let contacts = ContactStore::new(paths).list().unwrap_or_default();
            println!("  [OK] Whitelisted contacts: {}", contacts.len());

            if let Some(ref relay_url) = cfg.relay_url {
                if relay_url.starts_with("ws://")
                    && !relay_url.contains("127.0.0.1")
                    && !relay_url.contains("localhost")
                {
                    println!(
                        "  [WARN] Remote relay uses plaintext ws:// ({relay_url}) — use wss:// in production to protect metadata."
                    );
                }

                let health_url = relay_url
                    .replace("wss://", "https://")
                    .replace("ws://", "http://")
                    .trim_end_matches("/ws")
                    .to_string()
                    + "/health";
                match reqwest::Client::builder()
                    .timeout(Duration::from_secs(4))
                    .build()?
                    .get(&health_url)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        println!("  [OK] Optional relay reachable: {health_url}");
                    }
                    Ok(r) => println!(
                        "  [WARN] Optional relay returned HTTP {}: {health_url}",
                        r.status()
                    ),
                    Err(e) => println!(
                        "  [WARN] Optional relay unreachable ({e}): {health_url} (P2P still works)"
                    ),
                }
            } else {
                println!("  [OK] Relay          : none (pure zero-server P2P mode)");
            }

            let daemon_url = format!("http://{}/", cfg.daemon.ui_bind);
            match reqwest::Client::builder()
                .timeout(Duration::from_secs(1))
                .build()?
                .get(&daemon_url)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    println!("  [OK] Local daemon   : reachable at {daemon_url}");
                }
                Ok(r) => {
                    println!("  [WARN] Local daemon : HTTP {} at {daemon_url}", r.status());
                }
                Err(_) => {
                    println!(
                        "  [WARN] Local daemon : not reachable at {daemon_url} (start with `pa2pa service install` or `pa2pa daemon`)"
                    );
                }
            }
        }
        Err(_) => {
            println!("  [WARN] Not initialized yet — run `pa2pa init`.");
        }
    }

    println!("\nBackground services:");
    for s in service::inspect_services() {
        println!("  [{:<4}] {:<22} {}", s.status.tag(), s.name, s.detail);
    }

    println!("\nMCP registrations:");
    for m in setup_mcp::inspect_mcp_registrations(paths) {
        println!("  [{:<4}] {:<14} {}", m.status.tag(), m.tool, m.detail);
    }

    println!("\nDetected headless agent runners:");
    for r in detect_installed_runners() {
        let mark = if r.installed { "OK" } else { "--" };
        println!(
            "  [{:<4}] {:<18} ({:<8}) {}",
            mark,
            r.label,
            r.binary,
            r.path.unwrap_or_else(|| "not in PATH".into())
        );
    }

    let pill_bin = find_pill_binary();
    println!(
        "\nFloating desktop pill (`pa2pa-pill`): {}",
        pill_bin
            .map(|p| format!("OK ({})", p.display()))
            .unwrap_or_else(|| "not found next to pa2pa or in PATH".into())
    );
    Ok(())
}

async fn ping_local_daemon(paths: &Pa2paPaths, ui_bind: &str) {
    let Ok(token) = std::fs::read_to_string(paths.daemon_token_path()) else {
        return;
    };
    let url = format!("http://{ui_bind}/api/internal/refresh");
    if let Ok(c) = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        let _ = c
            .post(&url)
            .header("X-Pa2pa-Token", token.trim())
            .send()
            .await;
    }
}

pub(crate) fn find_pill_binary() -> Option<PathBuf> {
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            let sibling = dir.join(if cfg!(windows) {
                "pa2pa-pill.exe"
            } else {
                "pa2pa-pill"
            });
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    pa2pa_daemon::runners::find_in_path("pa2pa-pill")
}

fn spawn_pill_process(base_url: &str, start_hidden: bool) {
    if let Some(bin) = find_pill_binary() {
        let mut cmd = Command::new(&bin);
        cmd.arg("--url").arg(base_url);
        if start_hidden {
            cmd.arg("--start-hidden");
        }
        match cmd.spawn() {
            Ok(_) => println!("  Pill/Tray : launched ({})", bin.display()),
            Err(e) => eprintln!("Warning: failed to spawn pa2pa-pill: {e}"),
        }
    } else {
        eprintln!(
            "Warning: `pa2pa-pill` binary not found. Build/install it with `cargo build --bin pa2pa-pill` or open {base_url}/pill"
        );
    }
}

fn launch_pill_foreground(base_url: &str, start_hidden: bool) -> Result<()> {
    let bin = find_pill_binary().ok_or_else(|| {
        anyhow!("`pa2pa-pill` binary not found next to `pa2pa` or in PATH. Build it with `cargo build --bin pa2pa-pill`.")
    })?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--url").arg(base_url);
    if start_hidden {
        cmd.arg("--start-hidden");
    }
    let status = cmd.status()?;
    if !status.success() {
        bail!("pa2pa-pill exited with status {status}");
    }
    Ok(())
}
