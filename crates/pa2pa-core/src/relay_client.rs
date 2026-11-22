use crate::contacts::{Contact, ContactStore};
use crate::store::{Store, StoredMessage};
use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use pa2pa_crypto::{open_envelope, seal_envelope, EncryptedEnvelope, Identity, PeerPublicKey};
use pa2pa_proto::{
    AgentMessage, Attachment, AttachmentKind, ClientFrame, MessageKind, ServerFrame,
    MAX_ATTACHMENTS_BYTES,
};
use std::path::Path;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Load a file from disk into an E2E-encryptable `Attachment` (max 512 KiB).
pub fn load_attachment_from_path(path: &Path) -> Result<Attachment> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read attachment {}", path.display()))?;
    if bytes.len() > MAX_ATTACHMENTS_BYTES {
        bail!(
            "Attachment {} is {} bytes (max {} bytes)",
            path.display(),
            bytes.len(),
            MAX_ATTACHMENTS_BYTES
        );
    }
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment")
        .to_string();
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match ext.as_str() {
        "diff" | "patch" => AttachmentKind::Diff,
        "log" => AttachmentKind::Log,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "c" | "cpp" | "h" | "sql"
        | "toml" | "yaml" | "yml" | "json" | "sh" | "md" => AttachmentKind::Code,
        _ => AttachmentKind::File,
    };
    let (encoding, content, mime_type) = match String::from_utf8(bytes.clone()) {
        Ok(text) => (
            "utf8".to_string(),
            text,
            match kind {
                AttachmentKind::Diff => "text/x-diff",
                AttachmentKind::Log => "text/plain",
                AttachmentKind::Code => "text/plain",
                AttachmentKind::File => "text/plain",
            }
            .to_string(),
        ),
        Err(_) => (
            "base64".to_string(),
            B64.encode(&bytes),
            "application/octet-stream".to_string(),
        ),
    };
    Ok(Attachment {
        filename,
        kind,
        mime_type,
        encoding,
        content,
    })
}

/// Normalize a relay URL into a WebSocket endpoint ending with `/ws`.
pub fn normalize_relay_ws_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let ws = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    };
    if ws.ends_with("/ws") {
        ws
    } else {
        format!("{ws}/ws")
    }
}

pub struct SendResult {
    pub message: AgentMessage,
    pub delivered_live: bool,
}

/// Send an already-sealed `EncryptedEnvelope` via a mailbox relay WebSocket.
/// Returns `Ok(true)` if delivered live, `Ok(false)` if queued on the relay for offline pickup.
pub async fn send_sealed_envelope_via_relay(
    relay_url: &str,
    identity: &Identity,
    envelope: EncryptedEnvelope,
) -> Result<bool> {
    let url = normalize_relay_ws_url(relay_url);
    let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(&url))
        .await
        .with_context(|| format!("Timed out connecting to relay {url}"))?
        .with_context(|| format!("Failed to connect to relay {url}"))?;

    authenticate_ws(&mut ws, identity, None).await?;

    let send_frame = serde_json::to_string(&ClientFrame::Send { envelope })?;
    ws.send(WsMessage::Text(send_frame)).await?;

    let delivered_live = loop {
        let next = timeout(Duration::from_secs(10), ws.next())
            .await
            .context("Timed out waiting for relay send confirmation")?;
        match next {
            Some(Ok(WsMessage::Text(txt))) => match serde_json::from_str::<ServerFrame>(&txt)? {
                ServerFrame::Sent { delivered_live, .. } => break delivered_live,
                ServerFrame::Error { code, message } => {
                    bail!("Relay rejected message ({code}): {message}");
                }
                ServerFrame::Deliver { .. } => {
                    // Ignore queued inbound messages during a one-shot send (do not ack them).
                }
                _ => {}
            },
            Some(Ok(WsMessage::Close(_))) | None => {
                bail!("Relay closed connection before confirming send");
            }
            Some(Err(e)) => return Err(e.into()),
            _ => {}
        }
    };

    let _ = ws.close(None).await;
    Ok(delivered_live)
}

/// Connect once to a relay, authenticate with SSH Ed25519 challenge-response, and send an E2E encrypted message.
pub async fn send_encrypted_message(
    relay_url: &str,
    identity: &Identity,
    contact: &Contact,
    msg: AgentMessage,
    store: Option<&Store>,
) -> Result<SendResult> {
    let total_att_bytes: usize = msg.attachments.iter().map(|a| a.content.len()).sum();
    if total_att_bytes > MAX_ATTACHMENTS_BYTES {
        bail!(
            "Total attachments size ({total_att_bytes} bytes) exceeds limit ({MAX_ATTACHMENTS_BYTES} bytes)"
        );
    }
    let peer_key = contact.peer_public_key()?;
    let plaintext = serde_json::to_vec(&msg)?;
    let envelope = seal_envelope(
        identity,
        &peer_key,
        &msg.message_id,
        msg.created_at.timestamp(),
        &plaintext,
    )?;

    let target_relay = contact.relay.as_deref().unwrap_or(relay_url);
    let delivered_live = send_sealed_envelope_via_relay(target_relay, identity, envelope).await?;

    if let Some(store) = store {
        let stored = StoredMessage {
            id: msg.message_id.clone(),
            thread_id: msg.thread_id.clone(),
            in_reply_to: msg.in_reply_to.clone(),
            direction: "outbound".into(),
            peer_alias: contact.alias.clone(),
            peer_fingerprint: contact.fingerprint.clone(),
            kind: kind_to_str(&msg.kind).into(),
            subject: msg.subject.clone(),
            body: msg.body.clone(),
            attachments: msg.attachments.clone(),
            sender_agent: msg.sender_agent.clone(),
            auto_turn: msg.auto_turn,
            status: "sent".into(),
            created_at: msg.created_at,
        };
        store.insert_message(&stored)?;
    }

    Ok(SendResult {
        message: msg,
        delivered_live,
    })
}

/// Connect once to the relay, fetch & decrypt any pending messages from whitelisted contacts, ack them, and persist to local SQLite.
pub async fn pull_pending_messages(
    relay_url: &str,
    identity: &Identity,
    contacts: &ContactStore,
    store: &Store,
    sync_allowlist: bool,
) -> Result<Vec<(Contact, AgentMessage)>> {
    let url = normalize_relay_ws_url(relay_url);
    let (mut ws, _) = timeout(Duration::from_secs(10), connect_async(&url))
        .await
        .with_context(|| format!("Timed out connecting to relay {url}"))?
        .with_context(|| format!("Failed to connect to relay {url}"))?;

    let allowlist = if sync_allowlist {
        Some(contacts.allowed_fingerprints()?)
    } else {
        None
    };

    let pending_count = authenticate_ws(&mut ws, identity, allowlist).await?;
    let mut received = Vec::new();
    let mut processed = 0usize;

    while processed < pending_count {
        let next = timeout(Duration::from_secs(5), ws.next()).await;
        let Ok(Some(Ok(frame))) = next else {
            break;
        };
        if let WsMessage::Text(txt) = frame {
            if let Ok(ServerFrame::Deliver { envelope }) = serde_json::from_str::<ServerFrame>(&txt)
            {
                processed += 1;
                if let Some((contact, msg)) =
                    decrypt_and_store_inbound(identity, contacts, store, &envelope)?
                {
                    received.push((contact, msg));
                }
                let ack = serde_json::to_string(&ClientFrame::Ack {
                    message_id: envelope.message_id,
                })?;
                ws.send(WsMessage::Text(ack)).await?;
            }
        }
    }

    let _ = ws.close(None).await;
    Ok(received)
}

const MAX_PAST_SKEW_SECS: i64 = 30 * 24 * 3600;
const MAX_FUTURE_SKEW_SECS: i64 = 5 * 60;

fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Decrypt an incoming envelope only if its sender fingerprint matches a whitelisted contact.
/// Returns `Ok(None)` for unknown senders, invalid signatures, invalid IDs/timestamps, or replayed message IDs.
pub fn decrypt_and_store_inbound(
    identity: &Identity,
    contacts: &ContactStore,
    store: &Store,
    envelope: &EncryptedEnvelope,
) -> Result<Option<(Contact, AgentMessage)>> {
    let now = chrono::Utc::now().timestamp();
    if envelope.timestamp < now - MAX_PAST_SKEW_SECS
        || envelope.timestamp > now + MAX_FUTURE_SKEW_SECS
    {
        tracing::warn!(
            message_id = %envelope.message_id,
            ts = envelope.timestamp,
            "Dropping envelope with out-of-window timestamp"
        );
        return Ok(None);
    }

    if !is_safe_id(&envelope.message_id) {
        tracing::warn!("Dropping envelope with invalid message_id");
        return Ok(None);
    }

    let Some(contact) = contacts.find_by_fingerprint(&envelope.sender_fingerprint)? else {
        tracing::warn!(
            sender_fp = %envelope.sender_fingerprint,
            message_id = %envelope.message_id,
            "Dropping message from non-whitelisted SSH fingerprint"
        );
        return Ok(None);
    };

    let peer_key = contact.peer_public_key()?;
    let plaintext = match open_envelope(identity, &peer_key, envelope) {
        Ok(pt) => pt,
        Err(e) => {
            tracing::warn!(
                alias = %contact.alias,
                error = %e,
                "Failed to verify/decrypt envelope"
            );
            return Ok(None);
        }
    };

    let msg: AgentMessage = serde_json::from_slice(&plaintext)?;
    if msg.message_id != envelope.message_id {
        tracing::warn!(
            alias = %contact.alias,
            "Dropping envelope whose inner message_id does not match signed outer header"
        );
        return Ok(None);
    }
    if !is_safe_id(&msg.thread_id) {
        tracing::warn!(
            alias = %contact.alias,
            "Dropping message with invalid thread_id"
        );
        return Ok(None);
    }

    let stored = StoredMessage {
        id: msg.message_id.clone(),
        thread_id: msg.thread_id.clone(),
        in_reply_to: msg.in_reply_to.clone(),
        direction: "inbound".into(),
        peer_alias: contact.alias.clone(),
        peer_fingerprint: contact.fingerprint.clone(),
        kind: kind_to_str(&msg.kind).into(),
        subject: msg.subject.clone(),
        body: msg.body.clone(),
        attachments: msg.attachments.clone(),
        sender_agent: msg.sender_agent.clone(),
        auto_turn: msg.auto_turn,
        status: "unread".into(),
        created_at: msg.created_at,
    };

    let inserted = store.insert_message(&stored)?;
    if !inserted {
        tracing::debug!(
            message_id = %msg.message_id,
            alias = %contact.alias,
            "Ignoring replayed/duplicate message_id"
        );
        return Ok(None);
    }
    Ok(Some((contact, msg)))
}

pub async fn authenticate_ws<S>(
    ws: &mut S,
    identity: &Identity,
    allowed_senders: Option<Vec<String>>,
) -> Result<usize>
where
    S: SinkExt<WsMessage> + StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
    <S as futures_util::Sink<WsMessage>>::Error: std::error::Error + Send + Sync + 'static,
{
    let nonce = loop {
        let msg = timeout(Duration::from_secs(10), ws.next())
            .await
            .context("Timed out waiting for relay auth challenge")?
            .ok_or_else(|| anyhow!("Relay closed before sending challenge"))??;
        if let WsMessage::Text(txt) = msg {
            match serde_json::from_str::<ServerFrame>(&txt)? {
                ServerFrame::Challenge { nonce, .. } => break nonce,
                ServerFrame::Error { code, message } => bail!("Relay error ({code}): {message}"),
                _ => {}
            }
        }
    };

    let signature = identity.sign_relay_challenge(&nonce);
    let auth = serde_json::to_string(&ClientFrame::Auth {
        public_key: identity.openssh_public_key().to_string(),
        signature,
        allowed_senders,
    })?;
    ws.send(WsMessage::Text(auth)).await?;

    loop {
        let msg = timeout(Duration::from_secs(10), ws.next())
            .await
            .context("Timed out waiting for relay authentication")?
            .ok_or_else(|| anyhow!("Relay closed during authentication"))??;
        if let WsMessage::Text(txt) = msg {
            match serde_json::from_str::<ServerFrame>(&txt)? {
                ServerFrame::Authenticated { pending_count, .. } => return Ok(pending_count),
                ServerFrame::Error { code, message } => {
                    bail!("Relay authentication failed ({code}): {message}")
                }
                _ => {}
            }
        }
    }
}

/// Fetch Ed25519 SSH public keys for a GitHub user (`https://github.com/<username>.keys`).
pub async fn fetch_github_ed25519_keys(username: &str) -> Result<Vec<PeerPublicKey>> {
    let clean = username.trim().trim_start_matches('@');
    if clean.is_empty() || !clean.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("Invalid GitHub username `{username}`");
    }
    let url = format!("https://github.com/{clean}.keys");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("pa2pa/0.1")
        .build()?;
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("GitHub returned HTTP {} for {url}", resp.status());
    }
    let body = resp.text().await?;
    let mut keys = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("ssh-ed25519 ") {
            if let Ok(k) = PeerPublicKey::from_openssh(trimmed) {
                keys.push(k);
            }
        }
    }
    if keys.is_empty() {
        bail!("No ssh-ed25519 public key found at {url}");
    }
    Ok(keys)
}

pub fn kind_to_str(kind: &MessageKind) -> &'static str {
    match kind {
        MessageKind::Chat => "chat",
        MessageKind::ApprovalNotice => "approval_notice",
        MessageKind::Rejection => "rejection",
    }
}
