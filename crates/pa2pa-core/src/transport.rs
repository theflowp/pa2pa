use crate::config::Config;
use crate::contacts::Contact;
use crate::p2p::{bind_p2p_endpoint, peer_endpoint_id, send_p2p_envelope};
use crate::relay_client::{kind_to_str, send_sealed_envelope_via_relay};
use crate::store::{Store, StoredMessage};
use anyhow::{bail, Result};
use iroh::Endpoint;
use pa2pa_crypto::{seal_envelope, Identity};
use pa2pa_proto::{AgentMessage, MAX_ATTACHMENTS_BYTES};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMethod {
    /// Delivered directly peer-to-peer over QUIC (zero relay server involved).
    P2pDirect,
    /// Delivered live through a mailbox relay WebSocket.
    RelayLive,
    /// Stored encrypted on the mailbox relay (peer currently offline).
    RelayQueued,
    /// Peer offline in zero-server P2P mode; saved in local encrypted outbox for automatic retry by daemon.
    OutboxPending,
}

impl DeliveryMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::P2pDirect => "p2p_direct",
            Self::RelayLive => "relay_live",
            Self::RelayQueued => "relay_queued",
            Self::OutboxPending => "outbox_pending",
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::P2pDirect | Self::RelayLive)
    }
}

impl std::fmt::Display for DeliveryMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub struct DeliveryOutcome {
    pub message: AgentMessage,
    pub method: DeliveryMethod,
    pub detail: String,
}

/// Deliver an E2E encrypted message using Hybrid transport:
/// 1. Direct P2P QUIC (if enabled in `cfg.transport`)
/// 2. Optional Mailbox Relay fallback (if configured and enabled)
/// 3. Local Encrypted Outbox fallback (daemon automatically retries until peer comes online)
pub async fn deliver_message(
    cfg: &Config,
    identity: &Identity,
    contact: &Contact,
    msg: AgentMessage,
    store: &Store,
    warm_p2p: Option<&Endpoint>,
) -> Result<DeliveryOutcome> {
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

    let mut p2p_err: Option<String> = None;

    // 1. Try direct P2P QUIC first (zero server needed)
    if cfg.transport.p2p_enabled() {
        let peer_id = peer_endpoint_id(&peer_key)?;
        let try_res = if let Some(ep) = warm_p2p {
            send_p2p_envelope(ep, peer_id, &envelope, Duration::from_secs(6)).await
        } else {
            match bind_p2p_endpoint(identity).await {
                Ok(ep) => {
                    let r = send_p2p_envelope(&ep, peer_id, &envelope, Duration::from_secs(6)).await;
                    ep.close().await;
                    r
                }
                Err(e) => Err(e),
            }
        };

        match try_res {
            Ok(()) => {
                persist_outbound(store, contact, &msg, "sent")?;
                return Ok(DeliveryOutcome {
                    message: msg,
                    method: DeliveryMethod::P2pDirect,
                    detail: "Delivered directly peer-to-peer (QUIC E2E)".into(),
                });
            }
            Err(e) => {
                tracing::debug!(peer = %contact.alias, error = %e, "Direct P2P delivery unavailable");
                p2p_err = Some(e.to_string());
            }
        }
    }

    // 2. Fall back to optional mailbox relay if configured & enabled
    let effective_relay = contact.relay.as_deref().or(cfg.relay_url.as_deref());
    let mut relay_err: Option<String> = None;
    if cfg.transport.relay_enabled() {
        if let Some(relay_url) = effective_relay {
            match send_sealed_envelope_via_relay(relay_url, identity, envelope.clone()).await {
                Ok(delivered_live) => {
                    persist_outbound(store, contact, &msg, "sent")?;
                    let method = if delivered_live {
                        DeliveryMethod::RelayLive
                    } else {
                        DeliveryMethod::RelayQueued
                    };
                    let detail = if delivered_live {
                        "Delivered live via mailbox relay (E2E)".into()
                    } else {
                        "Queued encrypted on mailbox relay (peer offline)".into()
                    };
                    return Ok(DeliveryOutcome {
                        message: msg,
                        method,
                        detail,
                    });
                }
                Err(e) => {
                    tracing::warn!(peer = %contact.alias, error = %e, "Relay delivery failed");
                    relay_err = Some(e.to_string());
                }
            }
        }
    }

    // 3. Peer is offline (or relay unreachable): save sealed envelope in local encrypted outbox!
    let combined_err = match (p2p_err, relay_err) {
        (Some(p), Some(r)) => format!("P2P: {p}; Relay: {r}"),
        (Some(p), None) => format!("Peer offline (P2P): {p}"),
        (None, Some(r)) => format!("Relay unreachable: {r}"),
        (None, None) => "No transport available".to_string(),
    };

    if matches!(cfg.transport, crate::config::TransportMode::RelayOnly) && effective_relay.is_none()
    {
        bail!("Transport is `relay-only` but no relay URL is configured.");
    }

    persist_outbound(store, contact, &msg, "queued")?;
    store.enqueue_outbox(&contact.alias, &contact.fingerprint, &envelope, Some(&combined_err))?;

    Ok(DeliveryOutcome {
        message: msg,
        method: DeliveryMethod::OutboxPending,
        detail: "Peer currently offline — saved in local outbox; daemon will deliver automatically once online.".into(),
    })
}

fn persist_outbound(store: &Store, contact: &Contact, msg: &AgentMessage, status: &str) -> Result<()> {
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
        status: status.into(),
        created_at: msg.created_at,
    };
    store.insert_message(&stored)?;
    Ok(())
}
