use anyhow::{anyhow, bail, Context, Result};
use iroh::endpoint::{presets, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use pa2pa_crypto::{EncryptedEnvelope, Identity, PeerPublicKey};
use pa2pa_proto::{P2pFrame, PeerActivityState};
use std::time::Duration;
use tokio::time::timeout;

/// QUIC ALPN for direct peer-to-peer pa2pa exchanges.
pub const PA2PA_P2P_ALPN: &[u8] = b"pa2pa/p2p/1";
pub const MAX_P2P_FRAME_BYTES: usize = 1024 * 1024 + 8192;

/// Derive the Iroh `SecretKey` directly from the local OpenSSH Ed25519 seed.
/// This guarantees the user's P2P `EndpointId` equals their SSH Ed25519 public key.
pub fn iroh_secret_key(identity: &Identity) -> SecretKey {
    SecretKey::from_bytes(&identity.ed25519_seed_bytes())
}

/// Derive a peer's Iroh `EndpointId` directly from their OpenSSH Ed25519 public key.
pub fn peer_endpoint_id(peer: &PeerPublicKey) -> Result<EndpointId> {
    EndpointId::from_bytes(&peer.ed25519_public_bytes())
        .map_err(|e| anyhow!("Invalid Ed25519 key for P2P EndpointId: {e}"))
}

/// Derive the local Iroh `EndpointId` from our SSH identity.
pub fn local_endpoint_id(identity: &Identity) -> Result<EndpointId> {
    peer_endpoint_id(&identity.to_peer_public_key())
}

/// Bind a production P2P Iroh endpoint with DNS/Pkarr address lookup and QUIC hole-punching.
pub async fn bind_p2p_endpoint(identity: &Identity) -> Result<Endpoint> {
    let secret = iroh_secret_key(identity);
    let ep = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![PA2PA_P2P_ALPN.to_vec()])
        .bind()
        .await
        .context("Failed to bind Iroh P2P QUIC endpoint")?;
    Ok(ep)
}

/// Bind a minimal offline-capable P2P endpoint (great for local tests without external DNS).
pub async fn bind_local_p2p_endpoint(identity: &Identity) -> Result<Endpoint> {
    let secret = iroh_secret_key(identity);
    let ep = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![PA2PA_P2P_ALPN.to_vec()])
        .bind()
        .await
        .context("Failed to bind minimal Iroh P2P endpoint")?;
    Ok(ep)
}

/// Write a single JSON `P2pFrame` and close the send half of the stream.
pub async fn write_p2p_frame(send: &mut SendStream, frame: &P2pFrame) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    if bytes.len() > MAX_P2P_FRAME_BYTES {
        bail!("P2P frame exceeds max size ({MAX_P2P_FRAME_BYTES} bytes)");
    }
    send.write_all(&bytes)
        .await
        .context("Failed to write P2P frame")?;
    send.finish().context("Failed to finish P2P send stream")?;
    Ok(())
}

/// Read a single bounded JSON `P2pFrame` from a QUIC receive stream.
pub async fn read_p2p_frame(recv: &mut RecvStream) -> Result<P2pFrame> {
    let bytes = recv
        .read_to_end(MAX_P2P_FRAME_BYTES)
        .await
        .context("Failed to read P2P frame")?;
    let frame: P2pFrame =
        serde_json::from_slice(&bytes).context("Failed to parse P2P JSON frame")?;
    Ok(frame)
}

/// Deliver an E2E `EncryptedEnvelope` directly peer-to-peer over QUIC and wait for `Ack`.
pub async fn send_p2p_envelope(
    endpoint: &Endpoint,
    target: impl Into<EndpointAddr>,
    envelope: &EncryptedEnvelope,
    connect_timeout: Duration,
) -> Result<()> {
    let conn = timeout(connect_timeout, endpoint.connect(target, PA2PA_P2P_ALPN))
        .await
        .context("Timed out dialing peer directly via P2P")?
        .context("Direct P2P connection failed")?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .context("Failed to open P2P bidirectional stream")?;

    write_p2p_frame(
        &mut send,
        &P2pFrame::Deliver {
            envelope: envelope.clone(),
        },
    )
    .await?;

    let resp = timeout(Duration::from_secs(8), read_p2p_frame(&mut recv))
        .await
        .context("Timed out waiting for P2P Ack")??;

    conn.close(0u32.into(), b"done");

    match resp {
        P2pFrame::Ack { message_id } if message_id == envelope.message_id => Ok(()),
        P2pFrame::Error { code, message } => bail!("Peer rejected P2P message ({code}): {message}"),
        other => bail!("Unexpected P2P response: {other:?}"),
    }
}

/// Send a best-effort live typing/thinking activity signal over P2P.
pub async fn send_p2p_activity(
    endpoint: &Endpoint,
    target: impl Into<EndpointAddr>,
    thread_id: &str,
    state: PeerActivityState,
) -> Result<()> {
    let conn = timeout(
        Duration::from_secs(3),
        endpoint.connect(target, PA2PA_P2P_ALPN),
    )
    .await
    .context("Timed out dialing peer for activity")??;

    let (mut send, mut recv) = conn.open_bi().await?;
    write_p2p_frame(
        &mut send,
        &P2pFrame::Activity {
            thread_id: thread_id.to_string(),
            state,
        },
    )
    .await?;
    let _ = timeout(Duration::from_secs(2), read_p2p_frame(&mut recv)).await;
    conn.close(0u32.into(), b"done");
    Ok(())
}

/// Probe whether a whitelisted peer is currently reachable directly via P2P (`Ping` -> `Pong`).
pub async fn probe_p2p_peer(endpoint: &Endpoint, target: impl Into<EndpointAddr>) -> bool {
    let Ok(Ok(conn)) = timeout(
        Duration::from_secs(3),
        endpoint.connect(target, PA2PA_P2P_ALPN),
    )
    .await
    else {
        return false;
    };

    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        conn.close(0u32.into(), b"err");
        return false;
    };
    if write_p2p_frame(&mut send, &P2pFrame::Ping).await.is_err() {
        conn.close(0u32.into(), b"err");
        return false;
    }
    let ok = matches!(
        timeout(Duration::from_secs(2), read_p2p_frame(&mut recv)).await,
        Ok(Ok(P2pFrame::Pong))
    );
    conn.close(0u32.into(), b"done");
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa2pa_crypto::{open_envelope, seal_envelope};
    use pa2pa_proto::AgentMessage;

    #[tokio::test]
    async fn p2p_direct_delivery_and_ack_zero_relay() {
        let flo = Identity::generate("flo").unwrap();
        let benjamin = Identity::generate("benjamin").unwrap();

        let ep_flo = bind_local_p2p_endpoint(&flo).await.unwrap();
        let ep_ben = bind_local_p2p_endpoint(&benjamin).await.unwrap();

        // Confirm SSH Ed25519 public key == Iroh EndpointId deterministically
        assert_eq!(ep_flo.id(), local_endpoint_id(&flo).unwrap());
        assert_eq!(
            ep_ben.id(),
            peer_endpoint_id(&benjamin.to_peer_public_key()).unwrap()
        );

        let flo_addr = ep_flo.addr();
        let ben_pub = benjamin.to_peer_public_key();
        let flo_clone = flo.clone();

        // Flo accepts direct P2P QUIC stream, verifies remote_id == Benjamin's SSH key, decrypts, and ACKs
        let server_task = tokio::spawn(async move {
            let incoming = ep_flo.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            assert_eq!(conn.remote_id().as_bytes(), &ben_pub.ed25519_public_bytes());

            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let frame = read_p2p_frame(&mut recv).await.unwrap();
            match frame {
                P2pFrame::Deliver { envelope } => {
                    let pt = open_envelope(&flo_clone, &ben_pub, &envelope).unwrap();
                    let msg: AgentMessage = serde_json::from_slice(&pt).unwrap();
                    write_p2p_frame(
                        &mut send,
                        &P2pFrame::Ack {
                            message_id: envelope.message_id,
                        },
                    )
                    .await
                    .unwrap();
                    msg.body
                }
                other => panic!("expected Deliver, got {other:?}"),
            }
        });

        let msg = AgentMessage::new_chat(
            "benjamin",
            Some("claude".into()),
            "Direct zero-server P2P hello!",
            None,
            None,
            None,
            0,
        );
        let env = seal_envelope(
            &benjamin,
            &flo.to_peer_public_key(),
            &msg.message_id,
            msg.created_at.timestamp(),
            &serde_json::to_vec(&msg).unwrap(),
        )
        .unwrap();

        send_p2p_envelope(&ep_ben, flo_addr, &env, Duration::from_secs(5))
            .await
            .unwrap();

        let received_body = server_task.await.unwrap();
        assert_eq!(received_body, "Direct zero-server P2P hello!");
    }
}
