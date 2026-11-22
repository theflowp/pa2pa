use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use chrono::{DateTime, Utc};
use pa2pa_crypto::EncryptedEnvelope;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const INVITE_PREFIX: &str = "pa2pa://v1/";
pub const MAX_ATTACHMENTS_BYTES: usize = 512 * 1024;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("Invalid invite URI (expected prefix `{INVITE_PREFIX}`)")]
    InvalidInvitePrefix,
    #[error("Invalid base64url in invite URI: {0}")]
    InvalidBase64(#[from] base64::DecodeError),
    #[error("Invalid JSON payload: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Chat,
    ApprovalNotice,
    Rejection,
}

impl Default for MessageKind {
    fn default() -> Self {
        Self::Chat
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    File,
    Code,
    Diff,
    Log,
}

impl Default for AttachmentKind {
    fn default() -> Self {
        Self::File
    }
}

/// Encrypted attachment (code snippet, git diff, log excerpt, or small file).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    #[serde(default)]
    pub kind: AttachmentKind,
    #[serde(default = "default_mime")]
    pub mime_type: String,
    /// "utf8" or "base64"
    #[serde(default = "default_encoding")]
    pub encoding: String,
    pub content: String,
}

fn default_mime() -> String {
    "text/plain".into()
}

fn default_encoding() -> String {
    "utf8".into()
}

/// Live ephemeral activity status broadcast between whitelisted peers.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerActivityState {
    Thinking,
    Idle,
}

/// Decrypted end-to-end payload exchanged between personal agents.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMessage {
    pub message_id: String,
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub sender_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_agent: Option<String>,
    /// Incremented on every automated reply to stop infinite agent <-> agent loops.
    #[serde(default)]
    pub auto_turn: u32,
    pub created_at: DateTime<Utc>,
}

impl AgentMessage {
    pub fn new_chat(
        sender_name: impl Into<String>,
        sender_agent: Option<String>,
        body: impl Into<String>,
        subject: Option<String>,
        thread_id: Option<String>,
        in_reply_to: Option<String>,
        auto_turn: u32,
    ) -> Self {
        let message_id = Uuid::new_v4().to_string();
        let thread_id = thread_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        Self {
            message_id,
            thread_id,
            in_reply_to,
            kind: MessageKind::Chat,
            subject,
            body: body.into(),
            attachments: Vec::new(),
            sender_name: sender_name.into(),
            sender_agent,
            auto_turn,
            created_at: Utc::now(),
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }
}

/// Shareable 1-line invite token (`pa2pa://v1/<base64url>`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvitePayload {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub ssh_key: String,
    pub relay: String,
}

impl InvitePayload {
    pub fn encode(&self) -> Result<String, ProtoError> {
        let json = serde_json::to_vec(self)?;
        Ok(format!("{INVITE_PREFIX}{}", B64URL.encode(json)))
    }

    pub fn decode(uri: &str) -> Result<Self, ProtoError> {
        let trimmed = uri.trim();
        let b64 = trimmed
            .strip_prefix(INVITE_PREFIX)
            .ok_or(ProtoError::InvalidInvitePrefix)?;
        let bytes = B64URL.decode(b64)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Frames sent from client/daemon -> relay server over WebSocket.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    Auth {
        public_key: String,
        signature: String,
        /// Optional list of whitelisted sender fingerprints enforced at the relay edge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_senders: Option<Vec<String>>,
    },
    UpdateAllowlist {
        allowed_senders: Option<Vec<String>>,
    },
    Send {
        envelope: EncryptedEnvelope,
    },
    Ack {
        message_id: String,
    },
    /// Ask the relay which of your whitelisted contacts are currently connected online.
    CheckPresence {
        fingerprints: Vec<String>,
    },
    /// Ephemeral typing/thinking signal relayed live (never persisted).
    ActivitySignal {
        recipient_fingerprint: String,
        thread_id: String,
        state: PeerActivityState,
    },
    Ping,
}

/// Frames sent from relay server -> client/daemon over WebSocket.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Challenge {
        nonce: String,
        server_time: i64,
    },
    Authenticated {
        fingerprint: String,
        pending_count: usize,
    },
    Sent {
        message_id: String,
        delivered_live: bool,
    },
    Deliver {
        envelope: EncryptedEnvelope,
    },
    AllowlistUpdated {
        count: Option<usize>,
    },
    Presence {
        online: Vec<String>,
    },
    PeerActivity {
        sender_fingerprint: String,
        thread_id: String,
        state: PeerActivityState,
    },
    Pong,
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_roundtrip() {
        let inv = InvitePayload {
            name: "benjamin".into(),
            display_name: Some("Benjamin (Lead)".into()),
            ssh_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKey benjamin".into(),
            relay: "wss://relay.example.com/ws".into(),
        };
        let encoded = inv.encode().unwrap();
        assert!(encoded.starts_with(INVITE_PREFIX));
        let decoded = InvitePayload::decode(&encoded).unwrap();
        assert_eq!(inv, decoded);
    }
}
