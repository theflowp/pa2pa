use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use pa2pa_crypto::{verify_relay_challenge, EncryptedEnvelope};
use pa2pa_proto::{ClientFrame, ServerFrame};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, RwLock};
use tokio::time::timeout;
use uuid::Uuid;

const MAX_AUTH_FRAME_BYTES: usize = 64 * 1024;
const MAX_ALLOWLIST_ENTRIES: usize = 2048;
const MAX_SESSIONS_PER_IDENTITY: usize = 16;
const CLIENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub listen: String,
    pub db_path: PathBuf,
    pub ttl_seconds: i64,
    pub max_envelope_bytes: usize,
    pub max_pending_per_recipient: usize,
    /// When true, reject messages to recipients who have not yet registered an allowlist.
    pub strict_allowlist: bool,
    /// Per-identity sliding-window send limit per minute (0 = unlimited).
    pub max_sends_per_minute: u32,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8787".into(),
            db_path: PathBuf::from("pa2pa-relay.db"),
            ttl_seconds: 7 * 24 * 3600,
            max_envelope_bytes: 1024 * 1024,
            max_pending_per_recipient: 500,
            strict_allowlist: false,
            max_sends_per_minute: 60,
        }
    }
}

struct RelayStore {
    conn: Mutex<Connection>,
}

impl RelayStore {
    fn open(path: &PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS envelopes (
                 message_id TEXT NOT NULL,
                 recipient_fp TEXT NOT NULL,
                 sender_fp TEXT NOT NULL,
                 envelope_json TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 PRIMARY KEY (message_id, recipient_fp)
             );
             CREATE INDEX IF NOT EXISTS idx_envelopes_recipient ON envelopes(recipient_fp, created_at);
             CREATE INDEX IF NOT EXISTS idx_envelopes_expires ON envelopes(expires_at);
             CREATE TABLE IF NOT EXISTS allowlists (
                 recipient_fp TEXT PRIMARY KEY,
                 allowed_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn set_allowlist(&self, recipient_fp: &str, allowed: Option<&[String]>) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        match allowed {
            Some(list) => {
                let json = serde_json::to_string(list)?;
                conn.execute(
                    "INSERT INTO allowlists (recipient_fp, allowed_json, updated_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(recipient_fp) DO UPDATE SET allowed_json = excluded.allowed_json, updated_at = excluded.updated_at",
                    params![recipient_fp, json, Utc::now().timestamp()],
                )?;
            }
            None => {
                conn.execute(
                    "DELETE FROM allowlists WHERE recipient_fp = ?1",
                    params![recipient_fp],
                )?;
            }
        }
        Ok(())
    }

    fn is_sender_allowed(
        &self,
        recipient_fp: &str,
        sender_fp: &str,
        strict_allowlist: bool,
    ) -> Result<bool> {
        if recipient_fp == sender_fp {
            return Ok(true);
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let raw: Option<String> = conn
            .query_row(
                "SELECT allowed_json FROM allowlists WHERE recipient_fp = ?1",
                params![recipient_fp],
                |r| r.get(0),
            )
            .optional()?;
        let Some(json) = raw else {
            return Ok(!strict_allowlist);
        };
        let set: HashSet<String> = serde_json::from_str(&json).unwrap_or_default();
        Ok(set.contains(sender_fp))
    }

    fn store_envelope(
        &self,
        env: &EncryptedEnvelope,
        ttl_seconds: i64,
        max_pending: usize,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now().timestamp();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM envelopes WHERE recipient_fp = ?1 AND expires_at > ?2",
            params![env.recipient_fingerprint, now],
            |r| r.get(0),
        )?;
        if count as usize >= max_pending {
            return Ok(false);
        }
        let json = serde_json::to_string(env)?;
        conn.execute(
            "INSERT OR IGNORE INTO envelopes
             (message_id, recipient_fp, sender_fp, envelope_json, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                env.message_id,
                env.recipient_fingerprint,
                env.sender_fingerprint,
                json,
                now,
                now + ttl_seconds,
            ],
        )?;
        Ok(true)
    }

    fn list_pending(&self, recipient_fp: &str) -> Result<Vec<EncryptedEnvelope>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now().timestamp();
        let mut stmt = conn.prepare(
            "SELECT envelope_json FROM envelopes
             WHERE recipient_fp = ?1 AND expires_at > ?2
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![recipient_fp, now], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(env) = serde_json::from_str::<EncryptedEnvelope>(&row?) {
                out.push(env);
            }
        }
        Ok(out)
    }

    fn ack(&self, recipient_fp: &str, message_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "DELETE FROM envelopes WHERE recipient_fp = ?1 AND message_id = ?2",
            params![recipient_fp, message_id],
        )?;
        Ok(())
    }

    fn purge_expired(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now().timestamp();
        let deleted = conn.execute("DELETE FROM envelopes WHERE expires_at <= ?1", params![now])?;
        Ok(deleted)
    }

    fn total_queued(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row("SELECT COUNT(*) FROM envelopes", [], |r| r.get(0))
            .unwrap_or(0)
    }
}

struct RateLimiter {
    windows: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, key: &str, max_per_minute: u32) -> bool {
        if max_per_minute == 0 {
            return true;
        }
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let list = map.entry(key.to_string()).or_default();
        list.retain(|&t| t > cutoff);
        if list.len() >= max_per_minute as usize {
            return false;
        }
        list.push(now);
        true
    }
}

struct RelayState {
    config: RelayConfig,
    store: RelayStore,
    /// Map of fingerprint -> list of (session_id, bounded outbound_tx)
    clients: RwLock<HashMap<String, Vec<(String, mpsc::Sender<ServerFrame>)>>>,
    rate_limiter: RateLimiter,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    connected_identities: usize,
    queued_envelopes: i64,
}

fn build_router(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

pub async fn run_relay(config: RelayConfig) -> Result<()> {
    let (addr, server_handle) = spawn_relay(config).await?;
    tracing::info!(%addr, "pa2pa E2E relay listening (zero-knowledge)");
    server_handle.await??;
    Ok(())
}

/// Bind and spawn the relay server, returning the bound address and join handle (great for tests).
pub async fn spawn_relay(
    config: RelayConfig,
) -> Result<(SocketAddr, tokio::task::JoinHandle<Result<()>>)> {
    let store = RelayStore::open(&config.db_path)?;
    let state = Arc::new(RelayState {
        config: config.clone(),
        store,
        clients: RwLock::new(HashMap::new()),
        rate_limiter: RateLimiter::new(),
    });

    // Periodic expired envelope cleanup
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        loop {
            interval.tick().await;
            if let Ok(n) = cleanup_state.store.purge_expired() {
                if n > 0 {
                    tracing::debug!(purged = n, "Purged expired relay envelopes");
                }
            }
        }
    });

    let app = build_router(state);
    let listener = TcpListener::bind(&config.listen).await?;
    let addr = listener.local_addr()?;

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await?;
        Ok(())
    });

    Ok((addr, handle))
}

async fn health_handler(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let connected_identities = state.clients.read().await.len();
    let queued_envelopes = state.store.total_queued();
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        connected_identities,
        queued_envelopes,
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    let limit = state.config.max_envelope_bytes.saturating_add(4096);
    ws.max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<RelayState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // 1. Send cryptographically strong 256-bit challenge nonce
    let mut nonce_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = B64URL.encode(nonce_bytes);
    let challenge = ServerFrame::Challenge {
        nonce: nonce.clone(),
        server_time: Utc::now().timestamp(),
    };
    if send_frame(&mut ws_tx, &challenge).await.is_err() {
        return;
    }

    // 2. Expect bounded Auth frame within 10s
    let auth_msg = match timeout(Duration::from_secs(10), ws_rx.next()).await {
        Ok(Some(Ok(Message::Text(txt)))) if txt.len() <= MAX_AUTH_FRAME_BYTES => txt,
        _ => return,
    };

    let (fingerprint, allowed_senders) = match serde_json::from_str::<ClientFrame>(&auth_msg) {
        Ok(ClientFrame::Auth {
            public_key,
            signature,
            allowed_senders,
        }) => {
            if let Some(ref list) = allowed_senders {
                if list.len() > MAX_ALLOWLIST_ENTRIES {
                    let _ = send_frame(
                        &mut ws_tx,
                        &ServerFrame::Error {
                            code: "allowlist_too_large".into(),
                            message: format!("Allowlist exceeds max of {MAX_ALLOWLIST_ENTRIES} entries"),
                        },
                    )
                    .await;
                    return;
                }
            }
            match verify_relay_challenge(&public_key, &nonce, &signature) {
                Ok(peer) => (peer.fingerprint().to_string(), allowed_senders),
                Err(e) => {
                    let _ = send_frame(
                        &mut ws_tx,
                        &ServerFrame::Error {
                            code: "auth_failed".into(),
                            message: format!("SSH signature verification failed: {e}"),
                        },
                    )
                    .await;
                    return;
                }
            }
        }
        _ => {
            let _ = send_frame(
                &mut ws_tx,
                &ServerFrame::Error {
                    code: "expected_auth".into(),
                    message: "First frame must be `auth`".into(),
                },
            )
            .await;
            return;
        }
    };

    if let Some(ref list) = allowed_senders {
        let _ = state.store.set_allowlist(&fingerprint, Some(list));
    }

    let pending = state.store.list_pending(&fingerprint).unwrap_or_default();
    let pending_count = pending.len();

    // 3. Register bounded session channel (capped per identity)
    let session_id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel::<ServerFrame>(CLIENT_QUEUE_CAPACITY);
    {
        let mut clients = state.clients.write().await;
        let entry = clients.entry(fingerprint.clone()).or_default();
        if entry.len() >= MAX_SESSIONS_PER_IDENTITY {
            let _ = send_frame(
                &mut ws_tx,
                &ServerFrame::Error {
                    code: "too_many_sessions".into(),
                    message: format!("Exceeded max {MAX_SESSIONS_PER_IDENTITY} concurrent sessions"),
                },
            )
            .await;
            return;
        }
        entry.push((session_id.clone(), tx.clone()));
    }

    let _ = tx.try_send(ServerFrame::Authenticated {
        fingerprint: fingerprint.clone(),
        pending_count,
    });
    for env in pending {
        if tx.send(ServerFrame::Deliver { envelope: env }).await.is_err() {
            break;
        }
    }

    tracing::info!(fp = %fingerprint, pending = pending_count, "Peer authenticated on relay");

    // Writer task
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if send_frame(&mut ws_tx, &frame).await.is_err() {
                break;
            }
        }
    });

    // Reader loop
    while let Some(Ok(msg)) = ws_rx.next().await {
        match msg {
            Message::Text(txt) => {
                if txt.len() > state.config.max_envelope_bytes {
                    let _ = tx.try_send(ServerFrame::Error {
                        code: "payload_too_large".into(),
                        message: format!(
                            "Frame exceeds max size of {} bytes",
                            state.config.max_envelope_bytes
                        ),
                    });
                    continue;
                }
                match serde_json::from_str::<ClientFrame>(&txt) {
                    Ok(ClientFrame::Send { envelope }) => {
                        handle_send(&state, &fingerprint, &tx, envelope).await;
                    }
                    Ok(ClientFrame::Ack { message_id }) => {
                        let _ = state.store.ack(&fingerprint, &message_id);
                    }
                    Ok(ClientFrame::UpdateAllowlist { allowed_senders }) => {
                        if let Some(ref list) = allowed_senders {
                            if list.len() > MAX_ALLOWLIST_ENTRIES {
                                let _ = tx.try_send(ServerFrame::Error {
                                    code: "allowlist_too_large".into(),
                                    message: format!("Allowlist exceeds max of {MAX_ALLOWLIST_ENTRIES} entries"),
                                });
                                continue;
                            }
                        }
                        let count = allowed_senders.as_ref().map(|v| v.len());
                        let _ = state
                            .store
                            .set_allowlist(&fingerprint, allowed_senders.as_deref());
                        let _ = tx.try_send(ServerFrame::AllowlistUpdated { count });
                    }
                    Ok(ClientFrame::CheckPresence { fingerprints }) => {
                        let online = {
                            let clients = state.clients.read().await;
                            let mut found = Vec::new();
                            for fp in fingerprints.into_iter().take(512) {
                                if clients.contains_key(&fp)
                                    && state
                                        .store
                                        .is_sender_allowed(&fp, &fingerprint, state.config.strict_allowlist)
                                        .unwrap_or(false)
                                {
                                    found.push(fp);
                                }
                            }
                            found
                        };
                        let _ = tx.try_send(ServerFrame::Presence { online });
                    }
                    Ok(ClientFrame::ActivitySignal {
                        recipient_fingerprint,
                        thread_id,
                        state: activity_state,
                    }) => {
                        if state
                            .store
                            .is_sender_allowed(
                                &recipient_fingerprint,
                                &fingerprint,
                                state.config.strict_allowlist,
                            )
                            .unwrap_or(false)
                        {
                            let clients = state.clients.read().await;
                            if let Some(sessions) = clients.get(&recipient_fingerprint) {
                                for (_, rx_tx) in sessions {
                                    let _ = rx_tx.try_send(ServerFrame::PeerActivity {
                                        sender_fingerprint: fingerprint.clone(),
                                        thread_id: thread_id.clone(),
                                        state: activity_state.clone(),
                                    });
                                }
                            }
                        }
                    }
                    Ok(ClientFrame::Ping) => {
                        let _ = tx.try_send(ServerFrame::Pong);
                    }
                    Ok(ClientFrame::Auth { .. }) => {
                        let _ = tx.try_send(ServerFrame::Error {
                            code: "already_authenticated".into(),
                            message: "Session is already authenticated".into(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.try_send(ServerFrame::Error {
                            code: "invalid_frame".into(),
                            message: e.to_string(),
                        });
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup session
    {
        let mut clients = state.clients.write().await;
        if let Some(list) = clients.get_mut(&fingerprint) {
            list.retain(|(id, _)| id != &session_id);
            if list.is_empty() {
                clients.remove(&fingerprint);
            }
        }
    }
    writer.abort();
    tracing::debug!(fp = %fingerprint, "Peer disconnected from relay");
}

async fn handle_send(
    state: &RelayState,
    sender_fp: &str,
    sender_tx: &mpsc::Sender<ServerFrame>,
    envelope: EncryptedEnvelope,
) {
    if envelope.message_id.is_empty() || envelope.message_id.len() > 128 {
        let _ = sender_tx.try_send(ServerFrame::Error {
            code: "invalid_message_id".into(),
            message: "Invalid message_id length".into(),
        });
        return;
    }

    if envelope.sender_fingerprint != sender_fp {
        let _ = sender_tx.try_send(ServerFrame::Error {
            code: "sender_spoof".into(),
            message: "Envelope sender_fingerprint does not match authenticated SSH key".into(),
        });
        return;
    }

    if !state
        .rate_limiter
        .allow(sender_fp, state.config.max_sends_per_minute)
    {
        let _ = sender_tx.try_send(ServerFrame::Error {
            code: "rate_limited".into(),
            message: format!(
                "Rate limit exceeded ({} messages/minute)",
                state.config.max_sends_per_minute
            ),
        });
        return;
    }

    match state.store.is_sender_allowed(
        &envelope.recipient_fingerprint,
        sender_fp,
        state.config.strict_allowlist,
    ) {
        Ok(true) => {}
        Ok(false) => {
            let _ = sender_tx.try_send(ServerFrame::Error {
                code: "not_whitelisted".into(),
                message: "Recipient has not whitelisted your SSH key".into(),
            });
            return;
        }
        Err(e) => {
            let _ = sender_tx.try_send(ServerFrame::Error {
                code: "internal_error".into(),
                message: e.to_string(),
            });
            return;
        }
    }

    match state.store.store_envelope(
        &envelope,
        state.config.ttl_seconds,
        state.config.max_pending_per_recipient,
    ) {
        Ok(true) => {}
        Ok(false) => {
            let _ = sender_tx.try_send(ServerFrame::Error {
                code: "mailbox_full".into(),
                message: "Recipient mailbox queue is full".into(),
            });
            return;
        }
        Err(e) => {
            let _ = sender_tx.try_send(ServerFrame::Error {
                code: "storage_error".into(),
                message: e.to_string(),
            });
            return;
        }
    }

    let message_id = envelope.message_id.clone();
    let delivered_live = {
        let clients = state.clients.read().await;
        if let Some(sessions) = clients.get(&envelope.recipient_fingerprint) {
            let mut sent_any = false;
            for (_, rx_tx) in sessions {
                if rx_tx
                    .try_send(ServerFrame::Deliver {
                        envelope: envelope.clone(),
                    })
                    .is_ok()
                {
                    sent_any = true;
                }
            }
            sent_any
        } else {
            false
        }
    };

    let _ = sender_tx.try_send(ServerFrame::Sent {
        message_id,
        delivered_live,
    });
}

async fn send_frame(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    frame: &ServerFrame,
) -> Result<()> {
    let txt = serde_json::to_string(frame)?;
    ws_tx.send(Message::Text(txt)).await?;
    Ok(())
}
