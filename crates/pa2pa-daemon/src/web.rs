use crate::runners::{detect_installed_runners, run_agent_responder};
use crate::{DaemonEvent, DaemonState};
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::Stream;
use pa2pa_core::{
    build_safe_agent_prompt, fetch_github_ed25519_keys, send_encrypted_message, Config, Contact,
    ContactStore, ExecutionMode, RunnerKind, Store, TrustLevel,
};
use pa2pa_crypto::Identity;
use pa2pa_proto::{AgentMessage, Attachment, InvitePayload, MessageKind};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const PILL_HTML: &str = include_str!("../assets/pill.html");

pub fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/", get(serve_dashboard))
        .route("/pill", get(serve_pill))
        .route("/api/events", get(sse_events))
        .route("/api/internal/refresh", post(internal_refresh))
        .route("/api/status", get(get_status))
        .route("/api/contacts", get(list_contacts).post(create_contact))
        .route(
            "/api/contacts/:alias",
            put(update_contact).delete(delete_contact),
        )
        .route("/api/threads", get(list_threads))
        .route("/api/threads/:thread_id", get(get_thread))
        .route("/api/send", post(send_message))
        .route("/api/approvals", get(list_approvals))
        .route("/api/approvals/:id/approve", post(approve_request))
        .route("/api/approvals/:id/deny", post(deny_request))
        .route("/api/config", get(get_config).put(update_config))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_middleware,
        ))
        .with_state(state)
}

fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    if h == "localhost" || h == "127.0.0.1" || h == "[::1]" {
        return true;
    }
    if let Some((name, port)) = h.rsplit_once(':') {
        if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
            return matches!(name, "localhost" | "127.0.0.1" | "[::1]");
        }
    }
    false
}

fn is_loopback_origin(origin: &str) -> bool {
    let o = origin.trim().to_ascii_lowercase();
    let Some(rest) = o.strip_prefix("http://") else {
        return false;
    };
    is_loopback_host(rest)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn extract_query_token(query: Option<&str>) -> Option<&str> {
    let q = query?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == "token" {
                return Some(v);
            }
        }
    }
    None
}

async fn security_middleware(
    State(state): State<Arc<DaemonState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // 1. Block DNS rebinding via Host header check
    if let Some(host) = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()) {
        if !is_loopback_host(host) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Forbidden Host header (loopback only)" })),
            )
                .into_response();
        }
    }

    // 2. Block cross-origin browser requests via Origin header check
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        if !is_loopback_origin(origin) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "Forbidden Origin header" })),
            )
                .into_response();
        }
    }

    // 3. Require per-daemon bearer token on all /api/* endpoints
    let path = req.uri().path();
    if path.starts_with("/api/") {
        let header_token = req
            .headers()
            .get("x-pa2pa-token")
            .and_then(|v| v.to_str().ok());
        let sse_query_token = if path == "/api/events" {
            extract_query_token(req.uri().query())
        } else {
            None
        };
        let supplied = header_token.or(sse_query_token);
        let authorized = supplied
            .map(|t| constant_time_eq(t.as_bytes(), state.api_token.as_bytes()))
            .unwrap_or(false);
        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing or invalid X-Pa2pa-Token" })),
            )
                .into_response();
        }
    }

    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'self'",
        ),
    );
    resp
}

async fn serve_dashboard(State(state): State<Arc<DaemonState>>) -> Html<String> {
    let meta = format!(
        "<meta name=\"pa2pa-token\" content=\"{}\" />",
        state.api_token
    );
    Html(DASHBOARD_HTML.replace("<!--PA2PA_TOKEN_META-->", &meta))
}

async fn serve_pill(State(state): State<Arc<DaemonState>>) -> Html<String> {
    let meta = format!(
        "<meta name=\"pa2pa-token\" content=\"{}\" />",
        state.api_token
    );
    Html(PILL_HTML.replace("<!--PA2PA_TOKEN_META-->", &meta))
}

async fn sse_events(
    State(state): State<Arc<DaemonState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(ev) => {
            let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
            Some(Ok(Event::default().event(ev.kind).data(data)))
        }
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

async fn internal_refresh(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    state.emit(DaemonEvent::refresh("external_update"));
    Json(json!({ "ok": true }))
}

fn open_encrypted_store(paths: &pa2pa_core::Pa2paPaths) -> anyhow::Result<Store> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    Store::open_encrypted(&paths.db_path(), id)
}

async fn get_status(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    let cfg = match Config::load(&state.paths) {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let id = match Identity::load_openssh(&cfg.key_path) {
        Ok(i) => i,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let invite_uri = InvitePayload {
        name: cfg.name.clone(),
        display_name: Some(cfg.display_name.clone()),
        ssh_key: id.openssh_public_key().to_string(),
        relay: cfg.relay_url.clone(),
    }
    .encode()
    .unwrap_or_default();

    let contacts = ContactStore::new(&state.paths).list().unwrap_or_default();
    let contacts_count = contacts.len();
    let online_fps = state.online_fingerprints.read().await;
    let online_peers: Vec<String> = contacts
        .iter()
        .filter(|c| online_fps.contains(&c.fingerprint))
        .map(|c| c.alias.clone())
        .collect();
    drop(online_fps);
    let thinking_peers = state.thinking_peers.read().await.clone();

    let counts = Store::open_encrypted(&state.paths.db_path(), id.clone())
        .and_then(|s| s.counts())
        .unwrap_or_default();
    let relay_connected = state.relay_connected.load(Ordering::Relaxed);
    let relay_error = state.relay_error.read().await.clone();

    (
        StatusCode::OK,
        Json(json!({
            "name": cfg.name,
            "display_name": cfg.display_name,
            "fingerprint": id.fingerprint(),
            "ssh_public_key": id.openssh_public_key(),
            "relay_url": cfg.relay_url,
            "invite_uri": invite_uri,
            "ui_url": format!("http://{}", cfg.daemon.ui_bind),
            "relay_connected": relay_connected,
            "relay_error": relay_error,
            "contacts_count": contacts_count,
            "online_peers": online_peers,
            "thinking_peers": thinking_peers,
            "unread_messages": counts.unread_messages,
            "pending_approvals": counts.pending_approvals,
            "default_runner": cfg.daemon.default_runner,
            "os_sandbox": cfg.daemon.os_sandbox,
            "runners": detect_installed_runners(),
        })),
    )
}

async fn list_contacts(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    match ContactStore::new(&state.paths).list() {
        Ok(contacts) => (StatusCode::OK, Json(json!({ "contacts": contacts }))),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateContactBody {
    alias: Option<String>,
    display_name: Option<String>,
    ssh_key: Option<String>,
    invite_uri: Option<String>,
    github_username: Option<String>,
    relay: Option<String>,
    #[serde(default)]
    trust: TrustLevel,
    #[serde(default)]
    allowed_paths: Vec<String>,
    require_approval_for: Option<Vec<String>>,
    max_auto_turns: Option<u32>,
    runner: Option<RunnerKind>,
    #[serde(default)]
    prompt: String,
}

async fn create_contact(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<CreateContactBody>,
) -> impl IntoResponse {
    let (alias, display_name, ssh_key, relay) = if let Some(uri) = body
        .invite_uri
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match InvitePayload::decode(uri) {
            Ok(inv) => (
                body.alias.filter(|s| !s.trim().is_empty()).unwrap_or(inv.name),
                body.display_name.or(inv.display_name),
                inv.ssh_key,
                body.relay.or(Some(inv.relay)),
            ),
            Err(e) => return api_err(StatusCode::BAD_REQUEST, format!("Invalid invite URI: {e}")),
        }
    } else if let Some(gh) = body
        .github_username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match fetch_github_ed25519_keys(gh).await {
            Ok(keys) => (
                body.alias
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| gh.to_string()),
                body.display_name.or_else(|| Some(format!("@{gh}"))),
                keys[0].openssh_public_key().to_string(),
                body.relay,
            ),
            Err(e) => return api_err(StatusCode::BAD_REQUEST, e.to_string()),
        }
    } else {
        let Some(alias) = body.alias.filter(|s| !s.trim().is_empty()) else {
            return api_err(StatusCode::BAD_REQUEST, "Missing `alias`".into());
        };
        let Some(ssh_key) = body.ssh_key.filter(|s| !s.trim().is_empty()) else {
            return api_err(
                StatusCode::BAD_REQUEST,
                "Provide `ssh_key`, `invite_uri`, or `github_username`".into(),
            );
        };
        (alias, body.display_name, ssh_key, body.relay)
    };

    let mut contact = match Contact::new(alias, ssh_key, display_name, relay, body.trust, body.prompt)
    {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    contact.allowed_paths = body.allowed_paths;
    if let Some(req) = body.require_approval_for {
        contact.require_approval_for = req;
    }
    if let Some(mt) = body.max_auto_turns {
        contact.max_auto_turns = mt;
    }
    contact.runner = body.runner;

    if let Err(e) = ContactStore::new(&state.paths).save(&contact) {
        return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    state.allowlist_notify.notify_one();
    state.emit(DaemonEvent::refresh("contact_saved"));

    (StatusCode::CREATED, Json(json!({ "contact": contact })))
}

#[derive(Deserialize)]
struct UpdateContactBody {
    display_name: Option<String>,
    ssh_key: Option<String>,
    relay: Option<String>,
    trust: Option<TrustLevel>,
    allowed_paths: Option<Vec<String>>,
    require_approval_for: Option<Vec<String>>,
    max_auto_turns: Option<u32>,
    runner: Option<Option<RunnerKind>>,
    prompt: Option<String>,
}

async fn update_contact(
    State(state): State<Arc<DaemonState>>,
    AxumPath(alias): AxumPath<String>,
    Json(body): Json<UpdateContactBody>,
) -> impl IntoResponse {
    let cs = ContactStore::new(&state.paths);
    let mut contact = match cs.get(&alias) {
        Ok(Some(c)) => c,
        Ok(None) => return api_err(StatusCode::NOT_FOUND, format!("Contact `{alias}` not found")),
        Err(e) => return api_err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    if let Some(d) = body.display_name {
        contact.display_name = if d.trim().is_empty() { None } else { Some(d) };
    }
    if let Some(k) = body.ssh_key {
        match pa2pa_crypto::PeerPublicKey::from_openssh(&k) {
            Ok(peer) => {
                contact.ssh_key = peer.openssh_public_key().to_string();
                contact.fingerprint = peer.fingerprint().to_string();
            }
            Err(e) => return api_err(StatusCode::BAD_REQUEST, format!("Invalid SSH key: {e}")),
        }
    }
    if let Some(r) = body.relay {
        contact.relay = if r.trim().is_empty() { None } else { Some(r) };
    }
    if let Some(t) = body.trust {
        contact.trust = t;
    }
    if let Some(ap) = body.allowed_paths {
        contact.allowed_paths = ap;
    }
    if let Some(ra) = body.require_approval_for {
        contact.require_approval_for = ra;
    }
    if let Some(mt) = body.max_auto_turns {
        contact.max_auto_turns = mt;
    }
    if let Some(rn) = body.runner {
        contact.runner = rn;
    }
    if let Some(p) = body.prompt {
        contact.prompt = p;
    }

    if let Err(e) = cs.save(&contact) {
        return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    state.allowlist_notify.notify_one();
    state.emit(DaemonEvent::refresh("contact_updated"));
    (StatusCode::OK, Json(json!({ "contact": contact })))
}

async fn delete_contact(
    State(state): State<Arc<DaemonState>>,
    AxumPath(alias): AxumPath<String>,
) -> impl IntoResponse {
    match ContactStore::new(&state.paths).delete(&alias) {
        Ok(true) => {
            state.allowlist_notify.notify_one();
            state.emit(DaemonEvent::refresh("contact_deleted"));
            (StatusCode::OK, Json(json!({ "deleted": true })))
        }
        Ok(false) => api_err(StatusCode::NOT_FOUND, format!("Contact `{alias}` not found")),
        Err(e) => api_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn list_threads(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    match open_encrypted_store(&state.paths).and_then(|s| s.list_threads(50)) {
        Ok(threads) => (StatusCode::OK, Json(json!({ "threads": threads }))),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_thread(
    State(state): State<Arc<DaemonState>>,
    AxumPath(thread_id): AxumPath<String>,
) -> impl IntoResponse {
    let store = match open_encrypted_store(&state.paths) {
        Ok(s) => s,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let _ = store.mark_thread_read(&thread_id);
    match store.list_thread_messages(&thread_id) {
        Ok(messages) => {
            state.emit(DaemonEvent::refresh("thread_read"));
            (
                StatusCode::OK,
                Json(json!({ "thread_id": thread_id, "messages": messages })),
            )
        }
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct SendBody {
    to: String,
    message: String,
    subject: Option<String>,
    thread_id: Option<String>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

async fn send_message(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<SendBody>,
) -> impl IntoResponse {
    let cfg = match Config::load(&state.paths) {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let id = match Identity::load_openssh(&cfg.key_path) {
        Ok(i) => i,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let contact = match ContactStore::new(&state.paths).get(&body.to) {
        Ok(Some(c)) => c,
        Ok(None) => return api_err(StatusCode::NOT_FOUND, format!("Contact `{}` not found", body.to)),
        Err(e) => return api_err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let store = match Store::open_encrypted(&state.paths.db_path(), id.clone()) {
        Ok(s) => s,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        Some("human-ui".into()),
        body.message,
        body.subject,
        body.thread_id,
        None,
        0,
    )
    .with_attachments(body.attachments);

    match send_encrypted_message(&cfg.relay_url, &id, &contact, msg, Some(&store)).await {
        Ok(res) => {
            state.emit(DaemonEvent::refresh("message_sent"));
            (
                StatusCode::OK,
                Json(json!({
                    "sent": true,
                    "delivered_live": res.delivered_live,
                    "message_id": res.message.message_id,
                    "thread_id": res.message.thread_id
                })),
            )
        }
        Err(e) => api_err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[derive(Deserialize)]
struct ApprovalsQuery {
    status: Option<String>,
}

async fn list_approvals(
    State(state): State<Arc<DaemonState>>,
    Query(q): Query<ApprovalsQuery>,
) -> impl IntoResponse {
    match open_encrypted_store(&state.paths).and_then(|s| s.list_approvals(q.status.as_deref())) {
        Ok(approvals) => (StatusCode::OK, Json(json!({ "approvals": approvals }))),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct ApproveBody {
    /// "reply" (send `reply` or `proposed_reply`) or "run_agent" (invoke local agent runner now)
    #[serde(default = "default_approve_mode")]
    mode: String,
    reply: Option<String>,
    note: Option<String>,
    /// Optional execution mode override when running agent ("read-only" | "scoped-exec")
    execution_mode: Option<String>,
}

fn default_approve_mode() -> String {
    "reply".into()
}

async fn approve_request(
    State(state): State<Arc<DaemonState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<ApproveBody>,
) -> impl IntoResponse {
    let cfg = match Config::load(&state.paths) {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let identity = match Identity::load_openssh(&cfg.key_path) {
        Ok(i) => i,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let store = match Store::open_encrypted(&state.paths.db_path(), identity.clone()) {
        Ok(s) => s,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let approval = match store.get_approval(&id) {
        Ok(Some(a)) => a,
        Ok(None) => return api_err(StatusCode::NOT_FOUND, format!("Approval `{id}` not found")),
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let contact = match ContactStore::new(&state.paths).get(&approval.peer_alias) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return api_err(
                StatusCode::NOT_FOUND,
                format!("Contact `{}` no longer exists", approval.peer_alias),
            )
        }
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    let reply_text = if body.mode == "run_agent" {
        let history = store
            .list_thread_messages(&approval.thread_id)
            .unwrap_or_default();
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
        let exec_mode = match body.execution_mode.as_deref() {
            Some("scoped-exec") | Some("exec") => ExecutionMode::ScopedExec,
            _ => ExecutionMode::ReadOnly,
        };
        let prompt = build_safe_agent_prompt(&contact, &incoming, &history, &exec_mode);
        match run_agent_responder(
            &state.paths,
            &cfg,
            &contact,
            &exec_mode,
            &prompt,
            &approval.peer_alias,
        )
        .await
        {
            Ok(out) => out,
            Err(e) => {
                return api_err(
                    StatusCode::BAD_GATEWAY,
                    format!("Agent runner failed: {e}"),
                )
            }
        }
    } else {
        body.reply
            .filter(|s| !s.trim().is_empty())
            .or(approval.proposed_reply.clone())
            .unwrap_or_else(|| "Approved by owner.".to_string())
    };

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        Some(if body.mode == "run_agent" {
            cfg.daemon.default_runner.as_str().to_string()
        } else {
            "human-approved".to_string()
        }),
        reply_text.clone(),
        None,
        Some(approval.thread_id.clone()),
        Some(approval.message_id.clone()),
        0,
    );

    if let Err(e) = send_encrypted_message(&cfg.relay_url, &identity, &contact, msg, Some(&store)).await
    {
        return api_err(StatusCode::BAD_GATEWAY, format!("Failed to send reply: {e}"));
    }

    let _ = store.decide_approval(&id, "approved", body.note.as_deref());
    let _ = store.update_message_status(&approval.message_id, "replied");
    state.emit(DaemonEvent::refresh("approval_approved"));

    (
        StatusCode::OK,
        Json(json!({
            "approved": true,
            "approval_id": id,
            "reply_sent": reply_text
        })),
    )
}

#[derive(Deserialize)]
struct DenyBody {
    note: Option<String>,
    #[serde(default)]
    notify_peer: bool,
}

async fn deny_request(
    State(state): State<Arc<DaemonState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<DenyBody>,
) -> impl IntoResponse {
    let store = match open_encrypted_store(&state.paths) {
        Ok(s) => s,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let approval = match store.get_approval(&id) {
        Ok(Some(a)) => a,
        Ok(None) => return api_err(StatusCode::NOT_FOUND, format!("Approval `{id}` not found")),
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    if body.notify_peer {
        if let (Ok(cfg), Ok(Some(contact))) = (
            Config::load(&state.paths),
            ContactStore::new(&state.paths).get(&approval.peer_alias),
        ) {
            if let Ok(identity) = Identity::load_openssh(&cfg.key_path) {
                let mut msg = AgentMessage::new_chat(
                    cfg.name.clone(),
                    Some("human-denied".into()),
                    body.note
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "Request declined by owner.".to_string()),
                    None,
                    Some(approval.thread_id.clone()),
                    Some(approval.message_id.clone()),
                    0,
                );
                msg.kind = MessageKind::Rejection;
                let _ = send_encrypted_message(&cfg.relay_url, &identity, &contact, msg, Some(&store))
                    .await;
            }
        }
    }

    let _ = store.decide_approval(&id, "denied", body.note.as_deref());
    let _ = store.update_message_status(&approval.message_id, "denied");
    state.emit(DaemonEvent::refresh("approval_denied"));

    (
        StatusCode::OK,
        Json(json!({ "denied": true, "approval_id": id })),
    )
}

async fn get_config(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    match Config::load(&state.paths) {
        Ok(cfg) => (StatusCode::OK, Json(json!({ "config": cfg }))),
        Err(e) => api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct UpdateConfigBody {
    display_name: Option<String>,
    relay_url: Option<String>,
    default_runner: Option<RunnerKind>,
    default_workdir: Option<String>,
    notifications: Option<bool>,
    runner_timeout_secs: Option<u64>,
}

async fn update_config(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<UpdateConfigBody>,
) -> impl IntoResponse {
    let mut cfg = match Config::load(&state.paths) {
        Ok(c) => c,
        Err(e) => return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Some(d) = body.display_name {
        cfg.display_name = d;
    }
    if let Some(r) = body.relay_url {
        cfg.relay_url = r;
    }
    if let Some(runner) = body.default_runner {
        cfg.daemon.default_runner = runner;
    }
    if let Some(wd) = body.default_workdir {
        cfg.daemon.default_workdir = if wd.trim().is_empty() { None } else { Some(wd) };
    }
    if let Some(n) = body.notifications {
        cfg.daemon.notifications = n;
    }
    if let Some(t) = body.runner_timeout_secs {
        cfg.daemon.runner_timeout_secs = t.max(15);
    }
    if let Err(e) = cfg.save(&state.paths) {
        return api_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    state.emit(DaemonEvent::refresh("config_updated"));
    (StatusCode::OK, Json(json!({ "config": cfg })))
}

fn api_err(status: StatusCode, msg: String) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_origin_checks_block_dns_rebinding() {
        assert!(is_loopback_host("127.0.0.1:4789"));
        assert!(is_loopback_host("localhost:4789"));
        assert!(is_loopback_host("[::1]:4789"));
        assert!(!is_loopback_host("evil.com:4789"));
        assert!(!is_loopback_host("127.0.0.1.evil.com:4789"));

        assert!(is_loopback_origin("http://127.0.0.1:4789"));
        assert!(is_loopback_origin("http://localhost:4789"));
        assert!(!is_loopback_origin("https://evil.com"));
        assert!(!is_loopback_origin("http://localhost.evil.com"));
    }

    #[test]
    fn constant_time_token_comparison() {
        assert!(constant_time_eq(b"secret123", b"secret123"));
        assert!(!constant_time_eq(b"secret123", b"secret124"));
        assert!(!constant_time_eq(b"secret", b"secret123"));
    }
}
