pub mod notify;
pub mod runners;
pub mod web;

use anyhow::{bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use pa2pa_core::{
    authenticate_ws, decrypt_and_store_inbound, evaluate_incoming, normalize_relay_ws_url,
    parse_escalation, send_encrypted_message, Config, ContactStore, Pa2paPaths, PolicyDecision,
    Store,
};
use pa2pa_crypto::Identity;
use pa2pa_proto::{AgentMessage, ClientFrame, PeerActivityState, ServerFrame};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, Notify, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

#[derive(Clone, Debug, Serialize)]
pub struct DaemonEvent {
    pub kind: &'static str,
    pub detail: String,
}

impl DaemonEvent {
    pub fn refresh(detail: impl Into<String>) -> Self {
        Self {
            kind: "refresh",
            detail: detail.into(),
        }
    }

    pub fn approval(detail: impl Into<String>) -> Self {
        Self {
            kind: "approval",
            detail: detail.into(),
        }
    }

    pub fn message(detail: impl Into<String>) -> Self {
        Self {
            kind: "message",
            detail: detail.into(),
        }
    }
}

pub struct DaemonState {
    pub paths: Pa2paPaths,
    pub api_token: String,
    pub relay_connected: AtomicBool,
    pub relay_error: RwLock<Option<String>>,
    pub online_fingerprints: RwLock<HashSet<String>>,
    /// Map of peer alias -> active thread_id where peer's agent is currently thinking
    pub thinking_peers: RwLock<HashMap<String, String>>,
    /// Outbound channel into the active relay WebSocket session for ephemeral activity signals
    pub ws_tx: RwLock<Option<mpsc::UnboundedSender<ClientFrame>>>,
    pub events_tx: broadcast::Sender<DaemonEvent>,
    pub allowlist_notify: Notify,
}

impl DaemonState {
    pub fn new(paths: Pa2paPaths, api_token: String) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(128);
        Arc::new(Self {
            paths,
            api_token,
            relay_connected: AtomicBool::new(false),
            relay_error: RwLock::new(None),
            online_fingerprints: RwLock::new(HashSet::new()),
            thinking_peers: RwLock::new(HashMap::new()),
            ws_tx: RwLock::new(None),
            events_tx,
            allowlist_notify: Notify::new(),
        })
    }

    pub fn emit(&self, ev: DaemonEvent) {
        let _ = self.events_tx.send(ev);
    }

    pub async fn send_activity_signal(
        &self,
        recipient_fp: &str,
        thread_id: &str,
        state: PeerActivityState,
    ) {
        let tx = self.ws_tx.read().await.clone();
        if let Some(tx) = tx {
            let _ = tx.send(ClientFrame::ActivitySignal {
                recipient_fingerprint: recipient_fp.to_string(),
                thread_id: thread_id.to_string(),
                state,
            });
        }
    }
}

fn generate_and_persist_api_token(paths: &Pa2paPaths) -> Result<String> {
    paths.ensure_dirs()?;
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = B64URL.encode(bytes);
    let token_path = paths.daemon_token_path();

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&token_path)?;
        f.write_all(token.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&token_path, &token)?;
    }

    Ok(token)
}

pub async fn run_daemon(paths: Pa2paPaths, ui_bind_override: Option<String>) -> Result<()> {
    let cfg = Config::load(&paths)?;
    let _ = Identity::load_openssh(&cfg.key_path)?;
    let ui_bind = ui_bind_override.unwrap_or_else(|| cfg.daemon.ui_bind.clone());

    let parsed_addr: SocketAddr = ui_bind.parse().map_err(|e| {
        anyhow::anyhow!("Invalid daemon ui_bind `{ui_bind}` (expected loopback IP:port like 127.0.0.1:4789): {e}")
    })?;
    if !parsed_addr.ip().is_loopback() {
        bail!(
            "Refusing to bind local daemon control plane to non-loopback address `{ui_bind}`. Use 127.0.0.1:<port> or [::1]:<port>."
        );
    }

    let api_token = generate_and_persist_api_token(&paths)?;
    let state = DaemonState::new(paths.clone(), api_token);

    // Spawn persistent relay listener with auto-reconnect
    let relay_state = state.clone();
    tokio::spawn(async move {
        relay_loop(relay_state).await;
    });

    let app = web::router(state);
    let listener = TcpListener::bind(parsed_addr).await?;
    let addr = listener.local_addr()?;
    tracing::info!(
        dashboard = %format!("http://{addr}"),
        pill = %format!("http://{addr}/pill"),
        "pa2pa daemon running"
    );
    println!("pa2pa daemon listening (loopback-only, token-protected):");
    println!("  Dashboard : http://{addr}");
    println!("  Pill view : http://{addr}/pill");
    println!("  Relay     : {}", cfg.relay_url);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn relay_loop(state: Arc<DaemonState>) {
    let mut backoff = Duration::from_secs(1);

    loop {
        match connect_and_run_once(state.clone()).await {
            Ok(()) => {
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_secs = backoff.as_secs(), "Relay connection interrupted");
                *state.relay_error.write().await = Some(e.to_string());
            }
        }

        state.relay_connected.store(false, Ordering::Relaxed);
        state.emit(DaemonEvent::refresh("relay_disconnected"));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn connect_and_run_once(state: Arc<DaemonState>) -> Result<()> {
    let cfg = Config::load(&state.paths)?;
    let identity = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(&state.paths);
    let allowlist = if cfg.daemon.sync_allowlist_to_relay {
        Some(contacts.allowed_fingerprints()?)
    } else {
        None
    };

    let url = normalize_relay_ws_url(&cfg.relay_url);
    let (mut ws, _) = connect_async(&url).await?;
    authenticate_ws(&mut ws, &identity, allowlist).await?;

    state.relay_connected.store(true, Ordering::Relaxed);
    *state.relay_error.write().await = None;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientFrame>();
    *state.ws_tx.write().await = Some(out_tx.clone());
    state.emit(DaemonEvent::refresh("relay_connected"));
    tracing::info!(relay = %url, fp = %identity.fingerprint(), "Connected to pa2pa relay");

    // Request initial presence of whitelisted contacts
    if let Ok(fps) = contacts.allowed_fingerprints() {
        if !fps.is_empty() {
            let _ = out_tx.send(ClientFrame::CheckPresence { fingerprints: fps });
        }
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(25));
    ping_interval.tick().await;

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                let ping = serde_json::to_string(&ClientFrame::Ping)?;
                ws.send(WsMessage::Text(ping)).await?;
                if let Ok(fps) = ContactStore::new(&state.paths).allowed_fingerprints() {
                    if !fps.is_empty() {
                        let pres = serde_json::to_string(&ClientFrame::CheckPresence { fingerprints: fps })?;
                        ws.send(WsMessage::Text(pres)).await?;
                    }
                }
            }
            Some(frame) = out_rx.recv() => {
                let txt = serde_json::to_string(&frame)?;
                ws.send(WsMessage::Text(txt)).await?;
            }
            _ = state.allowlist_notify.notified() => {
                if let Ok(cfg_now) = Config::load(&state.paths) {
                    let fps = ContactStore::new(&state.paths).allowed_fingerprints().ok();
                    if cfg_now.daemon.sync_allowlist_to_relay {
                        let frame = serde_json::to_string(&ClientFrame::UpdateAllowlist { allowed_senders: fps.clone() })?;
                        ws.send(WsMessage::Text(frame)).await?;
                    }
                    if let Some(list) = fps {
                        if !list.is_empty() {
                            let pres = serde_json::to_string(&ClientFrame::CheckPresence { fingerprints: list })?;
                            ws.send(WsMessage::Text(pres)).await?;
                        }
                    }
                }
            }
            incoming = ws.next() => {
                match incoming {
                    Some(Ok(WsMessage::Text(txt))) => {
                        match serde_json::from_str::<ServerFrame>(&txt) {
                            Ok(ServerFrame::Deliver { envelope }) => {
                                let msg_id = envelope.message_id.clone();
                                let store = Store::open_encrypted(&state.paths.db_path(), identity.clone())?;
                                let contacts = ContactStore::new(&state.paths);
                                if let Some((contact, msg)) = decrypt_and_store_inbound(&identity, &contacts, &store, &envelope)? {
                                    let st = state.clone();
                                    tokio::spawn(async move {
                                        handle_inbound_message(st, contact, msg).await;
                                    });
                                }
                                let ack = serde_json::to_string(&ClientFrame::Ack { message_id: msg_id })?;
                                ws.send(WsMessage::Text(ack)).await?;
                            }
                            Ok(ServerFrame::Presence { online }) => {
                                let mut set = state.online_fingerprints.write().await;
                                set.clear();
                                for fp in online {
                                    set.insert(fp);
                                }
                                state.emit(DaemonEvent::refresh("presence_updated"));
                            }
                            Ok(ServerFrame::PeerActivity { sender_fingerprint, thread_id, state: act }) => {
                                if let Ok(Some(c)) = ContactStore::new(&state.paths).find_by_fingerprint(&sender_fingerprint) {
                                    let mut map = state.thinking_peers.write().await;
                                    match act {
                                        PeerActivityState::Thinking => {
                                            map.insert(c.alias.clone(), thread_id);
                                        }
                                        PeerActivityState::Idle => {
                                            map.remove(&c.alias);
                                        }
                                    }
                                    state.emit(DaemonEvent::refresh("peer_activity"));
                                }
                            }
                            Ok(ServerFrame::Error { code, message }) => {
                                tracing::warn!(%code, %message, "Relay server error");
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Err(e)) => {
                        *state.ws_tx.write().await = None;
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
        }
    }

    *state.ws_tx.write().await = None;
    Ok(())
}

async fn handle_inbound_message(
    state: Arc<DaemonState>,
    contact: pa2pa_core::Contact,
    msg: AgentMessage,
) {
    let Ok(cfg) = Config::load(&state.paths) else {
        return;
    };
    let Ok(identity) = Identity::load_openssh(&cfg.key_path) else {
        return;
    };
    let Ok(store) = Store::open_encrypted(&state.paths.db_path(), identity.clone()) else {
        return;
    };

    let history = store
        .list_thread_messages(&msg.thread_id)
        .unwrap_or_default();

    let decision = evaluate_incoming(&contact, &msg, &history);
    match decision {
        PolicyDecision::DropMuted => {
            tracing::info!(peer = %contact.alias, "Ignored message from muted contact");
        }
        PolicyDecision::RequireHumanApproval { reason } => {
            let _ = store.update_message_status(&msg.message_id, "pending_approval");
            if let Ok(rec) = store.create_approval(
                &msg.message_id,
                &msg.thread_id,
                &contact.alias,
                &msg.body,
                &reason,
                None,
                None,
            ) {
                state.emit(DaemonEvent::approval(format!(
                    "{}: {}",
                    contact.alias, rec.id
                )));
                if cfg.daemon.notifications {
                    notify::send_desktop_notification(
                        "pa2pa — Approval needed",
                        &format!("From {} ({})", contact.alias, contact.trust),
                        &msg.body,
                    )
                    .await;
                }
            }
        }
        PolicyDecision::AllowAutoRespond { mode, prompt } => {
            state.emit(DaemonEvent::message(format!(
                "Auto-handling message from {}",
                contact.alias
            )));
            state
                .send_activity_signal(
                    &contact.fingerprint,
                    &msg.thread_id,
                    PeerActivityState::Thinking,
                )
                .await;
            let title = msg
                .subject
                .as_deref()
                .unwrap_or(&contact.alias);

            let run_result =
                runners::run_agent_responder(&state.paths, &cfg, &contact, &mode, &prompt, title)
                    .await;
            state
                .send_activity_signal(
                    &contact.fingerprint,
                    &msg.thread_id,
                    PeerActivityState::Idle,
                )
                .await;

            match run_result {
                Ok(output) => {
                    if let Some(reason) = parse_escalation(&output) {
                        let _ = store.update_message_status(&msg.message_id, "pending_approval");
                        let _ = store.create_approval(
                            &msg.message_id,
                            &msg.thread_id,
                            &contact.alias,
                            &msg.body,
                            &reason,
                            None,
                            None,
                        );
                        state.emit(DaemonEvent::approval(format!(
                            "Escalated from {}: {}",
                            contact.alias, reason
                        )));
                        if cfg.daemon.notifications {
                            notify::send_desktop_notification(
                                "pa2pa — Agent escalated request",
                                &format!("From {}", contact.alias),
                                &reason,
                            )
                            .await;
                        }
                    } else if let Ok(identity) = Identity::load_openssh(&cfg.key_path) {
                        let reply = AgentMessage::new_chat(
                            cfg.name.clone(),
                            Some(
                                contact
                                    .runner
                                    .as_ref()
                                    .unwrap_or(&cfg.daemon.default_runner)
                                    .as_str()
                                    .to_string(),
                            ),
                            output,
                            None,
                            Some(msg.thread_id.clone()),
                            Some(msg.message_id.clone()),
                            msg.auto_turn.saturating_add(1),
                        );
                        if let Err(e) = send_encrypted_message(
                            &cfg.relay_url,
                            &identity,
                            &contact,
                            reply,
                            Some(&store),
                        )
                        .await
                        {
                            tracing::error!(error = %e, "Failed to send auto-reply");
                        } else {
                            let _ = store.update_message_status(&msg.message_id, "replied");
                            state.emit(DaemonEvent::message(format!(
                                "Auto-replied to {}",
                                contact.alias
                            )));
                            if cfg.daemon.notifications {
                                notify::send_desktop_notification(
                                    "pa2pa — Auto-replied",
                                    &format!("To {}", contact.alias),
                                    "Your agent answered automatically per contact policy.",
                                )
                                .await;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, peer = %contact.alias, "Auto-runner failed; falling back to human approval");
                    let reason = format!("Auto-responder unavailable ({e}) — please review and reply manually.");
                    let _ = store.update_message_status(&msg.message_id, "pending_approval");
                    let _ = store.create_approval(
                        &msg.message_id,
                        &msg.thread_id,
                        &contact.alias,
                        &msg.body,
                        &reason,
                        None,
                        None,
                    );
                    state.emit(DaemonEvent::approval(reason.clone()));
                    if cfg.daemon.notifications {
                        notify::send_desktop_notification(
                            "pa2pa — Action needed",
                            &format!("Message from {}", contact.alias),
                            &msg.body,
                        )
                        .await;
                    }
                }
            }
        }
    }
}
