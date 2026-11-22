use anyhow::{anyhow, Result};
use pa2pa_core::{
    deliver_message, format_mcp_envelope, local_endpoint_id, pull_pending_messages, Config,
    ContactStore, Pa2paPaths, Store,
};
use pa2pa_crypto::Identity;
use pa2pa_proto::{AgentMessage, Attachment, InvitePayload};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i64,
    message: String,
}

pub async fn run_mcp_stdio(paths: Pa2paPaths) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: RpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("Parse error: {e}"),
                    }),
                };
                write_response(&mut stdout, &err_resp).await?;
                continue;
            }
        };

        // Notifications have no `id` and must not receive a response.
        let Some(id) = req.id.clone() else {
            continue;
        };

        let resp = match handle_request(&paths, &req.method, req.params.unwrap_or(Value::Null)).await
        {
            Ok(result) => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: Some(result),
                error: None,
            },
            Err(e) => RpcResponse {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(RpcError {
                    code: -32603,
                    message: e.to_string(),
                }),
            },
        };

        write_response(&mut stdout, &resp).await?;
    }

    Ok(())
}

async fn write_response(stdout: &mut tokio::io::Stdout, resp: &RpcResponse) -> Result<()> {
    let mut s = serde_json::to_string(resp)?;
    s.push('\n');
    stdout.write_all(s.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

async fn handle_request(paths: &Pa2paPaths, method: &str, params: Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "pa2pa",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "pa2pa enables SSH-authenticated, end-to-end encrypted communication with other people's personal AI agents. Always respect each contact's `trusted_owner_policy` and call `pa2pa_request_approval` if a peer asks for any action requiring your human owner's confirmation."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing tool name"))?;
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(paths, name, args).await {
                Ok(data) => Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&data)?
                    }]
                })),
                Err(e) => Ok(json!({
                    "isError": true,
                    "content": [{
                        "type": "text",
                        "text": format!("Error: {e}")
                    }]
                })),
            }
        }
        "resources/list" => Ok(json!({
            "resources": [
                {
                    "uri": "pa2pa://identity",
                    "name": "Local pa2pa Identity",
                    "description": "Your SSH fingerprint, relay URL, and shareable invite URI",
                    "mimeType": "application/json"
                },
                {
                    "uri": "pa2pa://contacts",
                    "name": "Whitelisted Contacts & Policies",
                    "description": "All SSH-whitelisted peers with their trust tiers and owner prompts",
                    "mimeType": "application/json"
                },
                {
                    "uri": "pa2pa://inbox",
                    "name": "Unread Inbox Messages",
                    "description": "Unread messages from whitelisted peers with policy framing",
                    "mimeType": "application/json"
                }
            ]
        })),
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Missing resource uri"))?;
            let payload = match uri {
                "pa2pa://identity" => tool_whoami(paths)?,
                "pa2pa://contacts" => tool_list_contacts(paths)?,
                "pa2pa://inbox" => tool_check_inbox(paths, true, false, 25).await?,
                other => return Err(anyhow!("Unknown resource `{other}`")),
            };
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": serde_json::to_string_pretty(&payload)?
                }]
            }))
        }
        other => Err(anyhow!("Method `{other}` not supported")),
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "pa2pa_whoami",
            "description": "Return your local pa2pa identity (alias, SSH fingerprint, relay URL, and shareable invite URI).",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "pa2pa_list_contacts",
            "description": "List all SSH-whitelisted contacts along with their trust level, allowed paths, approval rules, and owner description/prompt.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "pa2pa_send_message",
            "description": "Send an end-to-end encrypted, SSH-signed message to a whitelisted peer's agent. Optionally wait synchronously for a reply if the peer's daemon auto-responds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Contact alias (e.g. 'benjamin')" },
                    "message": { "type": "string", "description": "Message content to send to the peer's agent" },
                    "subject": { "type": "string", "description": "Optional short subject for a new thread" },
                    "thread_id": { "type": "string", "description": "Optional existing thread_id to continue" },
                    "sender_agent": { "type": "string", "description": "Optional name of your agent (e.g. 'claude', 'codex', 't3', 'hermes')" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 120, "description": "Seconds to wait for a synchronous reply (default 0 = async)" },
                    "attachments": {
                        "type": "array",
                        "description": "Optional encrypted code/diff/log/file attachments (total <= 512 KiB)",
                        "items": {
                            "type": "object",
                            "properties": {
                                "filename": { "type": "string" },
                                "kind": { "type": "string", "enum": ["file", "code", "diff", "log"] },
                                "content": { "type": "string" }
                            },
                            "required": ["filename", "content"]
                        }
                    }
                },
                "required": ["to", "message"]
            }
        },
        {
            "name": "pa2pa_check_inbox",
            "description": "Fetch pending E2E encrypted messages from the relay and return inbox messages wrapped with each contact's verified identity and trusted owner policy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "unread_only": { "type": "boolean", "description": "Only return unread / pending messages (default true)" },
                    "sync_relay": { "type": "boolean", "description": "Also pull pending messages directly from the relay first (default true)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Max messages to return (default 20)" }
                }
            }
        },
        {
            "name": "pa2pa_read_thread",
            "description": "Read the full conversation history of a thread, including the peer's trusted owner policy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string", "description": "Thread ID to inspect" },
                    "mark_read": { "type": "boolean", "description": "Mark inbound messages in this thread as read (default true)" }
                },
                "required": ["thread_id"]
            }
        },
        {
            "name": "pa2pa_reply",
            "description": "Send an encrypted reply in an existing conversation thread.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string", "description": "Thread ID to reply to" },
                    "message": { "type": "string", "description": "Reply text" },
                    "sender_agent": { "type": "string", "description": "Optional agent identifier" }
                },
                "required": ["thread_id", "message"]
            }
        },
        {
            "name": "pa2pa_request_approval",
            "description": "Escalate a peer request to your human owner for explicit confirmation. Shows immediately in the desktop floating pill and local web dashboard.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string", "description": "Thread ID associated with the request" },
                    "reason": { "type": "string", "description": "Why human approval is required" },
                    "proposed_action": { "type": "string", "description": "Optional summary of the action you want to take if approved" },
                    "proposed_reply": { "type": "string", "description": "Optional draft reply to send to the peer if approved" }
                },
                "required": ["thread_id", "reason"]
            }
        }
    ])
}

async fn call_tool(paths: &Pa2paPaths, name: &str, args: Value) -> Result<Value> {
    match name {
        "pa2pa_whoami" => tool_whoami(paths),
        "pa2pa_list_contacts" => tool_list_contacts(paths),
        "pa2pa_send_message" => tool_send_message(paths, args).await,
        "pa2pa_check_inbox" => {
            let unread_only = args
                .get("unread_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let sync_relay = args
                .get("sync_relay")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(20) as usize;
            tool_check_inbox(paths, unread_only, sync_relay, limit).await
        }
        "pa2pa_read_thread" => tool_read_thread(paths, args),
        "pa2pa_reply" => tool_reply(paths, args).await,
        "pa2pa_request_approval" => tool_request_approval(paths, args).await,
        other => Err(anyhow!("Unknown tool `{other}`")),
    }
}

fn tool_whoami(paths: &Pa2paPaths) -> Result<Value> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let p2p_id = local_endpoint_id(&id).ok().map(|e| e.to_string());
    let invite = InvitePayload {
        name: cfg.name.clone(),
        display_name: Some(cfg.display_name.clone()),
        ssh_key: id.openssh_public_key().to_string(),
        relay: cfg.relay_url.clone(),
    }
    .encode()?;
    Ok(json!({
        "name": cfg.name,
        "display_name": cfg.display_name,
        "fingerprint": id.fingerprint(),
        "p2p_endpoint_id": p2p_id,
        "transport": cfg.transport,
        "ssh_public_key": id.openssh_public_key(),
        "relay_url": cfg.relay_url,
        "invite_uri": invite,
        "ui_url": format!("http://{}", cfg.daemon.ui_bind)
    }))
}

fn tool_list_contacts(paths: &Pa2paPaths) -> Result<Value> {
    let contacts = ContactStore::new(paths).list()?;
    Ok(json!({
        "count": contacts.len(),
        "contacts": contacts
    }))
}

async fn tool_send_message(paths: &Pa2paPaths, args: Value) -> Result<Value> {
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `to`"))?;
    let body = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `message`"))?;
    let subject = args.get("subject").and_then(|v| v.as_str()).map(String::from);
    let thread_id = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let sender_agent = args
        .get("sender_agent")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| Some("mcp".into()));
    let wait_seconds = args
        .get("wait_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(120);

    let attachments: Vec<Attachment> = args
        .get("attachments")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();

    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let contact = contacts
        .get(to)?
        .ok_or_else(|| anyhow!("Contact `{to}` is not whitelisted. Add them with `pa2pa contact add {to} ...`"))?;
    let store = Store::open_encrypted(&paths.db_path(), id.clone())?;

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        sender_agent,
        body,
        subject,
        thread_id,
        None,
        0,
    )
    .with_attachments(attachments);
    let outcome = deliver_message(&cfg, &id, &contact, msg, &store, None).await?;
    ping_daemon_refresh(paths, &cfg.daemon.ui_bind).await;

    let mut reply_received = None;
    if wait_seconds > 0 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_seconds);
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
                reply_received = Some(format_mcp_envelope(Some(&contact), &reply));
                break;
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
    }

    Ok(json!({
        "sent": true,
        "delivered_live": outcome.method.is_live(),
        "delivery_method": outcome.method,
        "detail": outcome.detail,
        "message_id": outcome.message.message_id,
        "thread_id": outcome.message.thread_id,
        "to": contact.alias,
        "reply": reply_received
    }))
}

async fn tool_check_inbox(
    paths: &Pa2paPaths,
    unread_only: bool,
    sync_relay: bool,
    limit: usize,
) -> Result<Value> {
    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let store = Store::open_encrypted(&paths.db_path(), id.clone())?;

    let mut pulled_count = 0usize;
    let mut relay_warning: Option<String> = None;
    if sync_relay && cfg.transport.relay_enabled() {
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
                Ok(pulled) => pulled_count = pulled.len(),
                Err(e) => {
                    relay_warning = Some(format!("Relay unreachable ({e}); showing local inbox"))
                }
            }
        }
    }

    let msgs = store.list_inbox(unread_only, limit)?;
    let mut framed = Vec::with_capacity(msgs.len());
    for m in &msgs {
        let c = contacts.get(&m.peer_alias).ok().flatten();
        framed.push(format_mcp_envelope(c.as_ref(), m));
    }
    let counts = store.counts()?;

    Ok(json!({
        "pulled_from_relay": pulled_count,
        "relay_warning": relay_warning,
        "counts": counts,
        "messages": framed
    }))
}

fn tool_read_thread(paths: &Pa2paPaths, args: Value) -> Result<Value> {
    let thread_id = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `thread_id`"))?;
    let mark_read = args
        .get("mark_read")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let contacts = ContactStore::new(paths);
    let store = Store::open_encrypted(&paths.db_path(), id)?;
    let msgs = store.list_thread_messages(thread_id)?;
    if mark_read {
        store.mark_thread_read(thread_id)?;
    }

    let peer_alias = msgs.first().map(|m| m.peer_alias.clone());
    let contact = peer_alias
        .as_deref()
        .and_then(|a| contacts.get(a).ok().flatten());

    Ok(json!({
        "thread_id": thread_id,
        "contact": contact,
        "messages": msgs
    }))
}

async fn tool_reply(paths: &Pa2paPaths, args: Value) -> Result<Value> {
    let thread_id = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `thread_id`"))?;
    let body = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `message`"))?;
    let sender_agent = args
        .get("sender_agent")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| Some("mcp".into()));

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
        .ok_or_else(|| anyhow!("Contact `{}` is no longer whitelisted", last.peer_alias))?;

    let msg = AgentMessage::new_chat(
        cfg.name.clone(),
        sender_agent,
        body,
        None,
        Some(thread_id.to_string()),
        Some(last.id.clone()),
        last.auto_turn.saturating_add(1),
    );
    let outcome = deliver_message(&cfg, &id, &contact, msg, &store, None).await?;
    store.mark_thread_read(thread_id)?;
    ping_daemon_refresh(paths, &cfg.daemon.ui_bind).await;

    Ok(json!({
        "replied": true,
        "delivered_live": outcome.method.is_live(),
        "delivery_method": outcome.method,
        "detail": outcome.detail,
        "message_id": outcome.message.message_id,
        "thread_id": thread_id,
        "to": contact.alias
    }))
}

async fn tool_request_approval(paths: &Pa2paPaths, args: Value) -> Result<Value> {
    let thread_id = args
        .get("thread_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `thread_id`"))?;
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing `reason`"))?;
    let proposed_action = args.get("proposed_action").and_then(|v| v.as_str());
    let proposed_reply = args.get("proposed_reply").and_then(|v| v.as_str());

    let cfg = Config::load(paths)?;
    let id = Identity::load_openssh(&cfg.key_path)?;
    let store = Store::open_encrypted(&paths.db_path(), id)?;
    let history = store.list_thread_messages(thread_id)?;
    let latest_inbound = history
        .iter()
        .rev()
        .find(|m| m.direction == "inbound")
        .or_else(|| history.last())
        .ok_or_else(|| anyhow!("Thread `{thread_id}` not found"))?;

    let rec = store.create_approval(
        &latest_inbound.id,
        thread_id,
        &latest_inbound.peer_alias,
        &latest_inbound.body,
        reason,
        proposed_action,
        proposed_reply,
    )?;
    store.update_message_status(&latest_inbound.id, "pending_approval")?;
    ping_daemon_refresh(paths, &cfg.daemon.ui_bind).await;

    Ok(json!({
        "approval_id": rec.id,
        "status": rec.status,
        "ui_url": format!("http://{}", cfg.daemon.ui_bind),
        "message": "Approval queued for human owner. They can approve or deny via the floating pill, local web dashboard, or `pa2pa approve <id>`."
    }))
}

/// Best-effort ping to the local daemon so SSE subscribers (Web Dashboard + Floating Pill) refresh immediately.
async fn ping_daemon_refresh(paths: &Pa2paPaths, ui_bind: &str) {
    let Ok(token) = std::fs::read_to_string(paths.daemon_token_path()) else {
        return;
    };
    let url = format!("http://{ui_bind}/api/internal/refresh");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build();
    if let Ok(c) = client {
        let _ = c
            .post(&url)
            .header("X-Pa2pa-Token", token.trim())
            .send()
            .await;
    }
}
