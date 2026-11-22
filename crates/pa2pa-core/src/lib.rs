pub mod config;
pub mod contacts;
pub mod p2p;
pub mod policy;
pub mod relay_client;
pub mod store;
pub mod transport;

pub use config::{Config, CustomRunnerConfig, DaemonConfig, Pa2paPaths, RunnerKind, TransportMode};
pub use contacts::{Contact, ContactStore, TrustLevel};
pub use p2p::{
    bind_local_p2p_endpoint, bind_p2p_endpoint, iroh_secret_key, local_endpoint_id,
    peer_endpoint_id, probe_p2p_peer, read_p2p_frame, send_p2p_activity, send_p2p_envelope,
    write_p2p_frame, write_p2p_response_and_flush, PA2PA_P2P_ALPN,
};
pub use policy::{
    build_safe_agent_prompt, evaluate_incoming, format_mcp_envelope, parse_escalation,
    ExecutionMode, PolicyDecision,
};
pub use relay_client::{
    authenticate_ws, decrypt_and_store_inbound, fetch_github_ed25519_keys,
    load_attachment_from_path, normalize_relay_ws_url, pull_pending_messages,
    send_encrypted_message, send_sealed_envelope_via_relay, SendResult,
};
pub use store::{ApprovalRecord, InboxCounts, OutboxEntry, Store, StoredMessage, ThreadSummary};
pub use transport::{deliver_message, DeliveryMethod, DeliveryOutcome};
