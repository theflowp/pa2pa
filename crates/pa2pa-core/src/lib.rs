pub mod config;
pub mod contacts;
pub mod policy;
pub mod relay_client;
pub mod store;

pub use config::{Config, CustomRunnerConfig, DaemonConfig, Pa2paPaths, RunnerKind};
pub use contacts::{Contact, ContactStore, TrustLevel};
pub use policy::{
    build_safe_agent_prompt, evaluate_incoming, format_mcp_envelope, parse_escalation,
    ExecutionMode, PolicyDecision,
};
pub use relay_client::{
    authenticate_ws, decrypt_and_store_inbound, fetch_github_ed25519_keys,
    load_attachment_from_path, normalize_relay_ws_url, pull_pending_messages,
    send_encrypted_message, SendResult,
};
pub use store::{ApprovalRecord, InboxCounts, Store, StoredMessage, ThreadSummary};
