pub mod notify;
pub mod runners;
pub mod web;

use anyhow::{bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use iroh::Endpoint;
use pa2pa_core::{
    authenticate_ws, bind_p2p_endpoint, decrypt_and_store_inbound, deliver_message,
    evaluate_incoming, local_endpoint_id, normalize_relay_ws_url, parse_escalation,
    peer_endpoint_id, read_p2p_frame, send_p2p_activity, send_p2p_envelope,
    send_sealed_envelope_via_relay, write_p2p_response_and_flush, Config, ContactStore, Pa2paPaths,
    PolicyDecision, Store,
};
use pa2pa_crypto::Identity;
use pa2pa_proto::{AgentMessage, ClientFrame, P2pFrame, PeerActivityState, ServerFrame};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, Notify, RwLock, Semaphore};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const MAX_CONCURRENT_P2P_STREAMS: usize = 32;

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
    pub p2p_ready: AtomicBool,
    pub p2p_endpoint_id: RwLock<Option<String>>,
    pub p2p_endpoint: RwLock<Option<Endpoint>>,
    pub relay_connected: AtomicBool,
    pub relay_error: RwLock<Option<String>>,
    pub online_fingerprints: RwLock<HashSet<String>>,
    /// Map of peer alias -> active thread_id where peer's agent is currently thinking
    pub thinking_peers: RwLock<HashMap<String, String>>,
    /// Outbound channel into the active relay WebSocket session for ephemeral activity signals
    pub ws_tx: RwLock<Option<mpsc::UnboundedSender<ClientFrame>>>,
    pub events_tx: broadcast::Sender<DaemonEvent>,
    pub allowlist_notify: Notify,
    pub outbox_notify: Notify,
}

impl DaemonState {
    pub fn new(paths: Pa2paPaths, api_token: String) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(128);
        Arc::new(Self {
            paths,
            api_token,
            p2p_ready: AtomicBool::new(false),
            p2p_endpoint_id: RwLock::new(None),
            p2p_endpoint: RwLock::new(None),
            relay_connected: AtomicBool::new(false),
            relay_error: RwLock::new(None),
            online_fingerprints: RwLock::new(HashSet::new()),
            thinking_peers: RwLock::new(HashMap::new()),
            ws_tx: RwLock::new(None),
            events_tx,
            allowlist_notify: Notify::new(),
            outbox_notify: Notify::new(),
        })
    }

    pub fn emit(&self, ev: DaemonEvent) {
        let _ = self.events_tx.send(ev);
    }

    pub async fn send_activity_signal(
        &self,
        contact: &pa2pa_core::Contact,
        thread_id: &str,
        state: PeerActivityState,
    ) {
        // Try P2P direct first if endpoint is active
        let ep_opt = self.p2p_endpoint.read().await.clone();
        if let Some(ep) = ep_opt {
            if let Ok(pk) = contact.peer_public_key() {
                if let Ok(peer_id) = peer_endpoint_id(&pk) {
                    if send_p2p_activity(&ep, peer_id, thread_id, state.clone())
                        .await
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }
        // Fallback to relay WebSocket if connected
        let tx = self.ws_tx.read().await.clone();
        if let Some(tx) = tx {
            let _ = tx.send(ClientFrame::ActivitySignal {
                recipient_fingerprint: contact.fingerprint.clone(),
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
    let identity = Identity::load_openssh(&cfg.key_path)?;
    let ui_bind = ui_bind_override.unwrap_or_else(|| cfg.daemon.ui_bind.clone());

    let parsed_addr: SocketAddr = ui_bind.parse().map_err(|e| {
        anyhow::anyhow!(
            "Invalid daemon ui_bind `{ui_bind}` (expected loopback IP:port like 127.0.0.1:4789): {e}"
        )
    })?;
    if !parsed_addr.ip().is_loopback() {
        bail!(
            "Refusing to bind local daemon control plane to non-loopback address `{ui_bind}`. Use 127.0.0.1:<port> or [::1]:<port>."
        );
    }

    let api_token = generate_and_persist_api_token(&paths)?;
    let state = DaemonState::new(paths.clone(), api_token);

    // 1. Start native zero-server P2P QUIC endpoint (when enabled)
    if cfg.transport.p2p_enabled() {
        match bind_p2p_endpoint(&identity).await {
            Ok(ep) => {
                let ep_id = ep.id().to_string();
                *state.p2p_endpoint_id.write().await = Some(ep_id.clone());
                *state.p2p_endpoint.write().await = Some(ep);
                state.p2p_ready.store(true, Ordering::Relaxed);
                tracing::info!(endpoint_id = %ep_id, "Native P2P QUIC endpoint listening (zero-server)");
                let p2p_state = state.clone();
                tokio::spawn(async move {
                    p2p_accept_loop(p2p_state).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "Could not bind P2P QUIC endpoint");
            }
        }
    } else if let Ok(id) = local_endpoint_id(&identity) {
        *state.p2p_endpoint_id.write().await = Some(id.to_string());
    }

    // 2. Start optional mailbox relay listener (auto-reconnects if relay_url is set)
    let relay_state = state.clone();
    tokio::spawn(async move {
        relay_loop(relay_state).await;
    });

    // 3. Start local encrypted outbox delivery worker (retries offline P2P peers automatically)
    let outbox_state = state.clone();
    tokio::spawn(async move {
        outbox_worker_loop(outbox_state).await;
    });

    let app = web::router(state.clone());
    let listener = TcpListener::bind(parsed_addr).await?;
    let addr = listener.local_addr()?;
    tracing::info!(
        dashboard = %format!("http://{addr}"),
        pill = %format!("http://{addr}/pill"),
        transport = %cfg.transport,
        "pa2pa daemon running"
    );
    println!("pa2pa daemon listening (loopback-only, token-protected):");
    println!("  Dashboard : http://{addr}");
    println!("  Pill view : http://{addr}/pill");
    println!("  Transport : {}", cfg.transport);
    if let Some(ep_id) = state.p2p_endpoint_id.read().await.as_deref() {
        println!("  P2P ID    : {ep_id}");
    }
    println!(
        "  Relay     : {}",
        cfg.relay_url
            .as_deref()
            .unwrap_or("(none — pure zero-server P2P)")
    );

    axum::serve(listener, app).await?;
    Ok(())
}

/// Accept incoming direct P2P QUIC connections, strictly gated by SSH Ed25519 contact whitelist,
/// bounded concurrency, and per-stage timeouts against slow-loris/DoS.
async fn p2p_accept_loop(state: Arc<DaemonState>) {
    let ep = match state.p2p_endpoint.read().await.clone() {
        Some(e) => e,
        None => return,
    };
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_P2P_STREAMS));

    while let Some(incoming) = ep.accept().await {
        let Ok(permit) = sem.clone().try_acquire_owned() else {
            tracing::warn!("Dropping incoming P2P connection: concurrency limit reached");
            continue;
        };
        let st = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(Ok(conn)) = timeout(Duration::from_secs(8), incoming).await else {
                return;
            };
            let remote_id = conn.remote_id();
            let remote_bytes = *remote_id.as_bytes();

            // Hard SSH whitelist check BEFORE reading any application frame!
            let contacts = ContactStore::new(&st.paths);
            let Ok(Some(contact)) = contacts.find_by_ed25519_bytes(&remote_bytes) else {
                tracing::warn!(
                    remote_id = %remote_id,
                    "Rejected P2P QUIC connection from non-whitelisted SSH Ed25519 key"
                );
                conn.close(1u32.into(), b"not_whitelisted");
                return;
            };
            if contact.trust == pa2pa_core::TrustLevel::Muted {
                tracing::debug!(peer = %contact.alias, "Rejected P2P connection from muted contact");
                conn.close(2u32.into(), b"muted");
                return;
            }

            // Mark peer online immediately
            {
                let mut online = st.online_fingerprints.write().await;
                online.insert(contact.fingerprint.clone());
            }
            st.emit(DaemonEvent::refresh("p2p_peer_seen"));

            let Ok(Ok((mut send, mut recv))) =
                timeout(Duration::from_secs(8), conn.accept_bi()).await
            else {
                return;
            };

            let frame = match timeout(Duration::from_secs(10), read_p2p_frame(&mut recv)).await {
                Ok(Ok(f)) => f,
                Ok(Err(e)) => {
                    tracing::debug!(peer = %contact.alias, error = %e, "Invalid P2P frame");
                    return;
                }
                Err(_) => {
                    tracing::debug!(peer = %contact.alias, "Timed out reading P2P frame");
                    return;
                }
            };

            match frame {
                P2pFrame::Ping => {
                    let _ = write_p2p_response_and_flush(&mut send, &P2pFrame::Pong).await;
                }
                P2pFrame::Activity { thread_id, state: act } => {
                    let safe_tid = !thread_id.is_empty()
                        && thread_id.len() <= 64
                        && thread_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                    if safe_tid {
                        let mut map = st.thinking_peers.write().await;
                        match act {
                            PeerActivityState::Thinking => {
                                map.insert(contact.alias.clone(), thread_id);
                            }
                            PeerActivityState::Idle => {
                                map.remove(&contact.alias);
                            }
                        }
                        drop(map);
                        st.emit(DaemonEvent::refresh("peer_activity"));
                    }
                    let _ = write_p2p_response_and_flush(&mut send, &P2pFrame::Pong).await;
                }
                P2pFrame::Deliver { envelope } => {
                    if envelope.sender_fingerprint != contact.fingerprint {
                        let _ = write_p2p_response_and_flush(
                            &mut send,
                            &P2pFrame::Error {
                                code: "sender_mismatch".into(),
                                message: "Envelope sender does not match authenticated QUIC peer".into(),
                            },
                        )
                        .await;
                        return;
                    }
                    let msg_id = envelope.message_id.clone();
                    let Ok(cfg) = Config::load(&st.paths) else {
                        return;
                    };
                    let Ok(identity) = Identity::load_openssh(&cfg.key_path) else {
                        return;
                    };
                    let Ok(store) = Store::open_encrypted(&st.paths.db_path(), identity.clone())
                    else {
                        return;
                    };
                    match decrypt_and_store_inbound(&identity, &contacts, &store, &envelope) {
                        Ok(Some((verified_contact, msg))) => {
                            let _ = write_p2p_response_and_flush(
                                &mut send,
                                &P2pFrame::Ack { message_id: msg_id },
                            )
                            .await;
                            let st_inner = st.clone();
                            tokio::spawn(async move {
                                handle_inbound_message(st_inner, verified_contact, msg).await;
                            });
                        }
                        Ok(None) => {
                            // Duplicate/replayed message ID — ack cleanly so sender stops retrying
                            let _ = write_p2p_response_and_flush(
                                &mut send,
                                &P2pFrame::Ack { message_id: msg_id },
                            )
                            .await;
                        }
                        Err(e) => {
                            let _ = write_p2p_response_and_flush(
                                &mut send,
                                &P2pFrame::Error {
                                    code: "decrypt_failed".into(),
                                    message: e.to_string(),
                                },
                            )
                            .await;
                        }
                    }
                }
                _ => {}
            }
        });
    }
}

/// Periodically retries delivering sealed envelopes in the local outbox (e.g. when a P2P peer was offline).
async fn outbox_worker_loop(state: Arc<DaemonState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(6));
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = state.outbox_notify.notified() => {}
        }

        let Ok(cfg) = Config::load(&state.paths) else {
            continue;
        };
        let Ok(identity) = Identity::load_openssh(&cfg.key_path) else {
            continue;
        };
        let Ok(store) = Store::open_encrypted(&state.paths.db_path(), identity.clone()) else {
            continue;
        };
        let _ = store.purge_expired_outbox(7);

        let Ok(due) = store.list_due_outbox(20) else {
            continue;
        };
        if due.is_empty() {
            continue;
        }

        let contacts = ContactStore::new(&state.paths);
        let ep_opt = state.p2p_endpoint.read().await.clone();

        for item in due {
            let Ok(Some(contact)) = contacts.get(&item.peer_alias) else {
                let _ = store.remove_outbox(&item.message_id);
                continue;
            };

            let mut delivered = false;
            let mut last_err = String::new();

            // 1. Try P2P direct
            if cfg.transport.p2p_enabled() {
                if let (Some(ref ep), Ok(pk)) = (&ep_opt, contact.peer_public_key()) {
                    if let Ok(peer_id) = peer_endpoint_id(&pk) {
                        match send_p2p_envelope(ep, peer_id, &item.envelope, Duration::from_secs(5))
                            .await
                        {
                            Ok(()) => delivered = true,
                            Err(e) => last_err = format!("P2P: {e}"),
                        }
                    }
                }
            }

            // 2. Fallback to optional relay
            if !delivered && cfg.transport.relay_enabled() {
                if let Some(relay_url) = contact.relay.as_deref().or(cfg.relay_url.as_deref()) {
                    match send_sealed_envelope_via_relay(relay_url, &identity, item.envelope.clone())
                        .await
                    {
                        Ok(_) => delivered = true,
                        Err(e) => {
                            if !last_err.is_empty() {
                                last_err.push_str("; ");
                            }
                            last_err.push_str(&format!("Relay: {e}"));
                        }
                    }
                }
            }

            if delivered {
                let _ = store.remove_outbox(&item.message_id);
                let _ = store.update_message_status(&item.message_id, "sent");
                tracing::info!(
                    message_id = %item.message_id,
                    peer = %item.peer_alias,
                    "Delivered queued outbox message"
                );
                state.emit(DaemonEvent::refresh("outbox_delivered"));
            } else {
                let _ = store.mark_outbox_retry(
                    &item.message_id,
                    item.attempts.saturating_add(1),
                    &last_err,
                );
            }
        }
    }
}

async fn relay_loop(state: Arc<DaemonState>) {
    let mut backoff = Duration::from_secs(1);

    loop {
        let should_connect = Config::load(&state.paths)
            .ok()
            .map(|c| c.transport.relay_enabled() && c.relay_url.is_some())
            .unwrap_or(false);

        if !should_connect {
            state.relay_connected.store(false, Ordering::Relaxed);
            *state.relay_error.write().await = None;
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

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
    let Some(relay_url) = cfg.relay_url.as_deref() else {
        return Ok(());
    };
    let identity = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(&state.paths);
    let allowlist = if cfg.daemon.sync_allowlist_to_relay {
        Some(contacts.allowed_fingerprints()?)
    } else {
        None
    };

    let url = normalize_relay_ws_url(relay_url);
    let (mut ws, _) = connect_async(&url).await?;
    authenticate_ws(&mut ws, &identity, allowlist).await?;

    state.relay_connected.store(true, Ordering::Relaxed);
    *state.relay_error.write().await = None;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientFrame>();
    *state.ws_tx.write().await = Some(out_tx.clone());
    state.emit(DaemonEvent::refresh("relay_connected"));
    tracing::info!(relay = %url, fp = %identity.fingerprint(), "Connected to optional pa2pa relay");

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
                .send_activity_signal(&contact, &msg.thread_id, PeerActivityState::Thinking)
                .await;
            let title = msg.subject.as_deref().unwrap_or(&contact.alias);

            let run_result =
                runners::run_agent_responder(&state.paths, &cfg, &contact, &mode, &prompt, title)
                    .await;
            state
                .send_activity_signal(&contact, &msg.thread_id, PeerActivityState::Idle)
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
                    } else {
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
                        let warm_ep = state.p2p_endpoint.read().await.clone();
                        match deliver_message(
                            &cfg,
                            &identity,
                            &contact,
                            reply,
                            &store,
                            warm_ep.as_ref(),
                        )
                        .await
                        {
                            Ok(outcome) => {
                                let _ = store.update_message_status(&msg.message_id, "replied");
                                state.outbox_notify.notify_one();
                                state.emit(DaemonEvent::message(format!(
                                    "Auto-replied to {} ({})",
                                    contact.alias, outcome.method
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
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to deliver auto-reply");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, peer = %contact.alias, "Auto-runner failed; falling back to human approval");
                    let reason = format!(
                        "Auto-responder unavailable ({e}) — please review and reply manually."
                    );
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
