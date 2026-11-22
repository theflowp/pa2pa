use pa2pa_core::{
    pull_pending_messages, send_encrypted_message, Contact, ContactStore, Pa2paPaths, Store,
    TrustLevel,
};
use pa2pa_crypto::Identity;
use pa2pa_proto::AgentMessage;
use pa2pa_relay::{spawn_relay, RelayConfig};
use std::fs;
use uuid::Uuid;

#[tokio::test]
async fn e2e_offline_store_and_forward_and_allowlist_rejection() {
    let base = std::env::temp_dir().join(format!("pa2pa-e2e-{}", Uuid::new_v4()));
    fs::create_dir_all(&base).unwrap();

    // 1. Start real zero-knowledge relay on ephemeral port
    let relay_cfg = RelayConfig {
        listen: "127.0.0.1:0".into(),
        db_path: base.join("relay.db"),
        ..RelayConfig::default()
    };
    let (addr, handle) = spawn_relay(relay_cfg).await.unwrap();
    let relay_url = format!("ws://{addr}/ws");

    // 2. Create three SSH Ed25519 identities: Flo, Benjamin (whitelisted), Eve (stranger)
    let flo_paths = Pa2paPaths {
        home: base.join("flo"),
    };
    let ben_paths = Pa2paPaths {
        home: base.join("benjamin"),
    };
    let eve_paths = Pa2paPaths {
        home: base.join("eve"),
    };
    flo_paths.ensure_dirs().unwrap();
    ben_paths.ensure_dirs().unwrap();
    eve_paths.ensure_dirs().unwrap();

    let flo_id = Identity::generate("flo").unwrap();
    let ben_id = Identity::generate("benjamin").unwrap();
    let eve_id = Identity::generate("eve").unwrap();

    let flo_contacts = ContactStore::new(&flo_paths);
    let ben_contacts = ContactStore::new(&ben_paths);
    let eve_contacts = ContactStore::new(&eve_paths);

    // Flo whitelists ONLY Benjamin
    let ben_contact = Contact::new(
        "benjamin",
        ben_id.openssh_public_key(),
        Some("Benjamin".into()),
        Some(relay_url.clone()),
        TrustLevel::AutoRead,
        "Trusted backend lead",
    )
    .unwrap();
    flo_contacts.save(&ben_contact).unwrap();

    // Benjamin & Eve both know Flo's public key
    let flo_contact = Contact::new(
        "flo",
        flo_id.openssh_public_key(),
        Some("Flo".into()),
        Some(relay_url.clone()),
        TrustLevel::Ask,
        "Teammate",
    )
    .unwrap();
    ben_contacts.save(&flo_contact).unwrap();
    eve_contacts.save(&flo_contact).unwrap();

    let flo_store = Store::open(&flo_paths.db_path()).unwrap();
    let ben_store = Store::open(&ben_paths.db_path()).unwrap();

    // 3. Flo connects once to sync allowlist to relay edge, then goes offline
    let initial = pull_pending_messages(&relay_url, &flo_id, &flo_contacts, &flo_store, true)
        .await
        .unwrap();
    assert!(initial.is_empty());

    // 4. Eve (NOT whitelisted) tries to send to Flo -> rejected at relay edge!
    let eve_msg = AgentMessage::new_chat("eve", Some("rogue".into()), "Sneaky prompt injection", None, None, None, 0);
    let eve_res = send_encrypted_message(&relay_url, &eve_id, &flo_contact, eve_msg, None).await;
    assert!(
        eve_res.is_err(),
        "Non-whitelisted sender must be rejected by relay allowlist"
    );

    // 5. Benjamin (whitelisted) sends E2E encrypted message while Flo is offline
    let ben_msg = AgentMessage::new_chat(
        "benjamin",
        Some("claude".into()),
        "Salut Flo! Peux-tu vérifier le schéma GraphQL ?",
        Some("GraphQL Schema".into()),
        None,
        None,
        0,
    );
    let sent = send_encrypted_message(&relay_url, &ben_id, &flo_contact, ben_msg, Some(&ben_store))
        .await
        .unwrap();
    assert!(!sent.delivered_live);

    // 6. Flo comes back online, pulls & decrypts the message
    let received = pull_pending_messages(&relay_url, &flo_id, &flo_contacts, &flo_store, true)
        .await
        .unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].0.alias, "benjamin");
    assert_eq!(
        received[0].1.body,
        "Salut Flo! Peux-tu vérifier le schéma GraphQL ?"
    );

    // 7. Subsequent pull returns 0 (message was ACKed and deleted from relay)
    let again = pull_pending_messages(&relay_url, &flo_id, &flo_contacts, &flo_store, true)
        .await
        .unwrap();
    assert!(again.is_empty());

    // 8. Replaying an already-seen signed envelope returns None (idempotent / replay-safe)
    let replay_msg = AgentMessage::new_chat(
        "benjamin",
        Some("claude".into()),
        "One-time request",
        None,
        None,
        None,
        0,
    );
    let replay_env = pa2pa_crypto::seal_envelope(
        &ben_id,
        &flo_id.to_peer_public_key(),
        &replay_msg.message_id,
        replay_msg.created_at.timestamp(),
        &serde_json::to_vec(&replay_msg).unwrap(),
    )
    .unwrap();
    let first = pa2pa_core::decrypt_and_store_inbound(&flo_id, &flo_contacts, &flo_store, &replay_env)
        .unwrap();
    assert!(first.is_some());
    let second = pa2pa_core::decrypt_and_store_inbound(&flo_id, &flo_contacts, &flo_store, &replay_env)
        .unwrap();
    assert!(second.is_none(), "Replayed envelope must not be processed twice");

    handle.abort();
    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn e2e_encrypted_at_rest_attachments_and_relay_rate_limit() {
    let base = std::env::temp_dir().join(format!("pa2pa-e2e-adv-{}", Uuid::new_v4()));
    fs::create_dir_all(&base).unwrap();

    // Relay with strict rate limit of 2 messages per minute
    let relay_cfg = RelayConfig {
        listen: "127.0.0.1:0".into(),
        db_path: base.join("relay.db"),
        max_sends_per_minute: 2,
        ..RelayConfig::default()
    };
    let (addr, handle) = spawn_relay(relay_cfg).await.unwrap();
    let relay_url = format!("ws://{addr}/ws");

    let flo_paths = Pa2paPaths {
        home: base.join("flo"),
    };
    let ben_paths = Pa2paPaths {
        home: base.join("benjamin"),
    };
    flo_paths.ensure_dirs().unwrap();
    ben_paths.ensure_dirs().unwrap();

    let flo_id = Identity::generate("flo").unwrap();
    let ben_id = Identity::generate("benjamin").unwrap();

    let flo_contacts = ContactStore::new(&flo_paths);
    let ben_contacts = ContactStore::new(&ben_paths);
    flo_contacts
        .save(
            &Contact::new(
                "benjamin",
                ben_id.openssh_public_key(),
                None,
                Some(relay_url.clone()),
                TrustLevel::Ask,
                "Trusted",
            )
            .unwrap(),
        )
        .unwrap();
    let flo_contact = Contact::new(
        "flo",
        flo_id.openssh_public_key(),
        None,
        Some(relay_url.clone()),
        TrustLevel::Ask,
        "Trusted",
    )
    .unwrap();
    ben_contacts.save(&flo_contact).unwrap();

    let flo_store = Store::open_encrypted(&flo_paths.db_path(), flo_id.clone()).unwrap();
    let ben_store = Store::open_encrypted(&ben_paths.db_path(), ben_id.clone()).unwrap();

    // 1. Send message #1 with a code attachment
    let secret_phrase = "SUPER_SECRET_MIGRATION_TOKEN_XYZ_987";
    let msg1 = AgentMessage::new_chat(
        "benjamin",
        Some("claude".into()),
        secret_phrase,
        Some("Patch review".into()),
        None,
        None,
        0,
    )
    .with_attachments(vec![pa2pa_proto::Attachment {
        filename: "fix.diff".into(),
        kind: pa2pa_proto::AttachmentKind::Diff,
        mime_type: "text/x-diff".into(),
        encoding: "utf8".into(),
        content: "+fn secure() -> bool { true }\n".into(),
    }]);
    send_encrypted_message(&relay_url, &ben_id, &flo_contact, msg1, Some(&ben_store))
        .await
        .unwrap();

    // 2. Send message #2 (still within rate limit 2/min)
    let msg2 = AgentMessage::new_chat("benjamin", None, "Second msg", None, None, None, 0);
    send_encrypted_message(&relay_url, &ben_id, &flo_contact, msg2, Some(&ben_store))
        .await
        .unwrap();

    // 3. Send message #3 -> must be rejected by relay rate limiter!
    let msg3 = AgentMessage::new_chat("benjamin", None, "Flood msg", None, None, None, 0);
    let rate_res =
        send_encrypted_message(&relay_url, &ben_id, &flo_contact, msg3, Some(&ben_store)).await;
    assert!(
        rate_res.is_err(),
        "Third message should be rate-limited by relay"
    );

    // 4. Flo pulls & decrypts message #1 + attachment
    let pulled = pull_pending_messages(&relay_url, &flo_id, &flo_contacts, &flo_store, true)
        .await
        .unwrap();
    assert_eq!(pulled.len(), 2);
    assert_eq!(pulled[0].1.attachments.len(), 1);
    assert_eq!(pulled[0].1.attachments[0].filename, "fix.diff");

    // 5. Verify encryption-at-rest: raw SQLite file must NOT contain plaintext secret!
    let raw_db_bytes = fs::read(flo_paths.db_path()).unwrap();
    let raw_db_lossy = String::from_utf8_lossy(&raw_db_bytes);
    assert!(
        !raw_db_lossy.contains(secret_phrase),
        "SQLite DB at rest must not store message body in plaintext"
    );

    // 6. Reading via encrypted Store returns the decrypted message & attachment cleanly
    let inbox = flo_store.list_inbox(false, 10).unwrap();
    let m1 = inbox.iter().find(|m| m.body == secret_phrase).unwrap();
    assert_eq!(m1.attachments.len(), 1);
    assert_eq!(m1.attachments[0].content, "+fn secure() -> bool { true }\n");

    handle.abort();
    let _ = fs::remove_dir_all(base);
}
