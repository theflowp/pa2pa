use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ssh_key::private::{Ed25519Keypair, KeypairData};
use ssh_key::public::{Ed25519PublicKey, KeyData};
use ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey as SshPublicKey};
use std::fs;
use std::io::Write;
use std::path::Path;
use thiserror::Error;
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey, StaticSecret};
use zeroize::Zeroize;

const RELAY_AUTH_DOMAIN: &[u8] = b"pa2pa-relay-auth-v1:";
const E2E_KEY_DOMAIN: &[u8] = b"pa2pa-e2e-xchacha20poly1305-v1";
const ENVELOPE_SIG_DOMAIN: &[u8] = b"pa2pa-envelope-sig-v1";
const LOCAL_AT_REST_DOMAIN: &[u8] = b"pa2pa-local-at-rest-v1";
pub const AT_REST_PREFIX: &str = "enc:v1:";
pub const ENVELOPE_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SSH key parsing error: {0}")]
    SshKey(String),
    #[error("SSH private key is encrypted with a passphrase; set PA2PA_SSH_PASSPHRASE or run `pa2pa init --dedicated-key`")]
    EncryptedKeyUnsupported,
    #[error("Invalid passphrase for encrypted SSH private key")]
    InvalidPassphrase,
    #[error("Only Ed25519 SSH keys are supported (found {0})")]
    UnsupportedKeyType(String),
    #[error("Invalid base64 encoding: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("Invalid key or signature length")]
    InvalidLength,
    #[error("Invalid Ed25519 signature")]
    InvalidSignature,
    #[error("Envelope version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("Envelope recipient fingerprint mismatch")]
    RecipientMismatch,
    #[error("Envelope sender fingerprint mismatch")]
    SenderMismatch,
    #[error("AEAD encryption or decryption failed")]
    AeadFailure,
    #[error("Low-order X25519 public key rejected")]
    WeakSharedSecret,
}

pub type Result<T> = std::result::Result<T, CryptoError>;

/// Local Ed25519 identity backed by an OpenSSH keypair.
#[derive(Clone)]
pub struct Identity {
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
    openssh_public: String,
    fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .field("openssh_public", &self.openssh_public)
            .finish()
    }
}

/// Verified remote peer public key parsed from an OpenSSH `ssh-ed25519` string.
#[derive(Clone, Debug)]
pub struct PeerPublicKey {
    verifying_key: VerifyingKey,
    openssh_public: String,
    fingerprint: String,
}

/// End-to-end encrypted and Ed25519-signed wire envelope.
/// The relay server sees only routing fingerprints and opaque ciphertext.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncryptedEnvelope {
    pub version: u8,
    pub message_id: String,
    pub timestamp: i64,
    pub sender_fingerprint: String,
    pub recipient_fingerprint: String,
    pub ephemeral_pubkey: String,
    pub nonce: String,
    pub ciphertext: String,
    pub signature: String,
}

impl Identity {
    /// Generate a fresh Ed25519 identity.
    pub fn generate(comment: &str) -> Result<Self> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::from_signing_key(signing_key, comment)
    }

    pub fn from_signing_key(signing_key: SigningKey, comment: &str) -> Result<Self> {
        let verifying_key = signing_key.verifying_key();
        let ssh_pub = SshPublicKey::new(
            KeyData::Ed25519(Ed25519PublicKey(verifying_key.to_bytes())),
            comment,
        );
        let openssh_public = ssh_pub
            .to_openssh()
            .map_err(|e| CryptoError::SshKey(e.to_string()))?;
        let fingerprint = ssh_pub.fingerprint(HashAlg::Sha256).to_string();

        Ok(Self {
            signing_key,
            verifying_key,
            openssh_public,
            fingerprint,
        })
    }

    /// Save the private key in standard OpenSSH format (atomically 0600 on Unix) and `.pub` alongside it.
    pub fn save_openssh(&self, private_path: &Path, comment: &str) -> Result<()> {
        if let Some(parent) = private_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut seed = self.signing_key.to_bytes();
        let kp = Ed25519Keypair::from_seed(&seed);
        seed.zeroize();
        let private_key = PrivateKey::new(KeypairData::Ed25519(kp), comment)
            .map_err(|e| CryptoError::SshKey(e.to_string()))?;
        let pem = private_key
            .to_openssh(LineEnding::LF)
            .map_err(|e| CryptoError::SshKey(e.to_string()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(private_path)?;
            f.write_all(pem.as_bytes())?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            fs::write(private_path, pem.as_bytes())?;
        }

        let pub_path = if private_path.extension().is_none() {
            let mut os = private_path.as_os_str().to_os_string();
            os.push(".pub");
            std::path::PathBuf::from(os)
        } else {
            private_path.with_extension("pub")
        };
        fs::write(&pub_path, format!("{}\n", self.openssh_public))?;

        Ok(())
    }

    /// Load an Ed25519 OpenSSH private key (supports encrypted keys via `PA2PA_SSH_PASSPHRASE`).
    pub fn load_openssh(private_path: &Path) -> Result<Self> {
        let env_pass = std::env::var("PA2PA_SSH_PASSPHRASE").ok();
        Self::load_openssh_with_passphrase(private_path, env_pass.as_deref())
    }

    /// Load an Ed25519 OpenSSH private key, decrypting with `passphrase` if encrypted.
    pub fn load_openssh_with_passphrase(
        private_path: &Path,
        passphrase: Option<&str>,
    ) -> Result<Self> {
        let raw = fs::read_to_string(private_path)?;
        let mut private_key =
            PrivateKey::from_openssh(&raw).map_err(|e| CryptoError::SshKey(e.to_string()))?;

        if private_key.is_encrypted() {
            let Some(pass) = passphrase.filter(|s| !s.is_empty()) else {
                return Err(CryptoError::EncryptedKeyUnsupported);
            };
            private_key = private_key
                .decrypt(pass.as_bytes())
                .map_err(|_| CryptoError::InvalidPassphrase)?;
        }

        let comment = private_key.comment().to_string();
        match private_key.key_data() {
            KeypairData::Ed25519(kp) => {
                let mut seed: [u8; 32] = kp.private.to_bytes();
                let signing_key = SigningKey::from_bytes(&seed);
                seed.zeroize();
                Self::from_signing_key(signing_key, &comment)
            }
            other => Err(CryptoError::UnsupportedKeyType(format!(
                "{:?}",
                other.algorithm()
            ))),
        }
    }

    /// Derive a 256-bit local storage encryption key for encrypting SQLite message bodies at rest.
    pub fn derive_local_storage_key(&self) -> [u8; 32] {
        let secret = self.x25519_secret();
        let mut hasher = Sha256::new();
        hasher.update(LOCAL_AT_REST_DOMAIN);
        hasher.update(secret.to_bytes());
        hasher.update(self.verifying_key.as_bytes());
        let digest = hasher.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        key
    }

    /// Encrypt a UTF-8 string for local SQLite storage at rest (`enc:v1:<nonce>:<ct>`).
    pub fn encrypt_local_text(&self, plaintext: &str) -> Result<String> {
        let mut key = self.derive_local_storage_key();
        let cipher = XChaCha20Poly1305::new((&key).into());
        key.zeroize();

        let mut nonce_bytes = [0u8; 24];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .map_err(|_| CryptoError::AeadFailure)?;
        Ok(format!(
            "{AT_REST_PREFIX}{}:{}",
            B64.encode(nonce_bytes),
            B64.encode(ct)
        ))
    }

    /// Decrypt a string previously encrypted with `encrypt_local_text`.
    /// Transparently returns legacy plaintext rows that do not start with `enc:v1:`.
    pub fn decrypt_local_text(&self, stored: &str) -> Result<String> {
        let Some(rest) = stored.strip_prefix(AT_REST_PREFIX) else {
            return Ok(stored.to_string());
        };
        let Some((nonce_b64, ct_b64)) = rest.split_once(':') else {
            return Err(CryptoError::AeadFailure);
        };
        let nonce_bytes: [u8; 24] = B64
            .decode(nonce_b64)?
            .try_into()
            .map_err(|_| CryptoError::InvalidLength)?;
        let ct = B64.decode(ct_b64)?;

        let mut key = self.derive_local_storage_key();
        let cipher = XChaCha20Poly1305::new((&key).into());
        key.zeroize();

        let pt = cipher
            .decrypt(XNonce::from_slice(&nonce_bytes), ct.as_slice())
            .map_err(|_| CryptoError::AeadFailure)?;
        String::from_utf8(pt).map_err(|_| CryptoError::AeadFailure)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn openssh_public_key(&self) -> &str {
        &self.openssh_public
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    /// Raw 32-byte Ed25519 public key (identical to the peer's Iroh P2P EndpointId).
    pub fn ed25519_public_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// Raw 32-byte Ed25519 private seed (for deriving matching P2P QUIC endpoint identity).
    pub fn ed25519_seed_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn to_peer_public_key(&self) -> PeerPublicKey {
        PeerPublicKey {
            verifying_key: self.verifying_key,
            openssh_public: self.openssh_public.clone(),
            fingerprint: self.fingerprint.clone(),
        }
    }

    /// Derive the corresponding X25519 static secret scalar from Ed25519 signing key.
    pub fn x25519_secret(&self) -> StaticSecret {
        StaticSecret::from(self.signing_key.to_scalar_bytes())
    }

    /// Sign a relay WebSocket authentication challenge nonce.
    pub fn sign_relay_challenge(&self, nonce: &str) -> String {
        let mut msg = Vec::with_capacity(RELAY_AUTH_DOMAIN.len() + nonce.len());
        msg.extend_from_slice(RELAY_AUTH_DOMAIN);
        msg.extend_from_slice(nonce.as_bytes());
        let sig = self.signing_key.sign(&msg);
        B64.encode(sig.to_bytes())
    }
}

impl PeerPublicKey {
    /// Parse an OpenSSH public key line (`ssh-ed25519 AAAA... [optional comment]`).
    pub fn from_openssh(line: &str) -> Result<Self> {
        let trimmed = line.trim();
        let ssh_pub = SshPublicKey::from_openssh(trimmed)
            .map_err(|e| CryptoError::SshKey(e.to_string()))?;
        let fingerprint = ssh_pub.fingerprint(HashAlg::Sha256).to_string();
        match ssh_pub.key_data() {
            KeyData::Ed25519(pk) => {
                let verifying_key = VerifyingKey::from_bytes(&pk.0)
                    .map_err(|e| CryptoError::SshKey(e.to_string()))?;
                let canonical = ssh_pub
                    .to_openssh()
                    .map_err(|e| CryptoError::SshKey(e.to_string()))?;
                Ok(Self {
                    verifying_key,
                    openssh_public: canonical,
                    fingerprint,
                })
            }
            other => Err(CryptoError::UnsupportedKeyType(format!(
                "{:?}",
                other.algorithm()
            ))),
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn openssh_public_key(&self) -> &str {
        &self.openssh_public
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    /// Raw 32-byte Ed25519 public key (identical to the peer's Iroh P2P EndpointId).
    pub fn ed25519_public_bytes(&self) -> [u8; 32] {
        self.verifying_key.to_bytes()
    }

    /// Convert Ed25519 verifying key (Edwards) to X25519 public key (Montgomery).
    pub fn x25519_public(&self) -> X25519PublicKey {
        X25519PublicKey::from(self.verifying_key.to_montgomery().to_bytes())
    }
}

/// Verify a relay challenge-response signature and return the authenticated peer public key.
pub fn verify_relay_challenge(
    openssh_public: &str,
    nonce: &str,
    signature_b64: &str,
) -> Result<PeerPublicKey> {
    let peer = PeerPublicKey::from_openssh(openssh_public)?;
    let sig_bytes = B64.decode(signature_b64)?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let sig = Signature::from_bytes(&sig_arr);

    let mut msg = Vec::with_capacity(RELAY_AUTH_DOMAIN.len() + nonce.len());
    msg.extend_from_slice(RELAY_AUTH_DOMAIN);
    msg.extend_from_slice(nonce.as_bytes());

    peer.verifying_key
        .verify(&msg, &sig)
        .map_err(|_| CryptoError::InvalidSignature)?;

    Ok(peer)
}

fn derive_symmetric_key(
    shared_secret: &[u8; 32],
    eph_pub: &[u8; 32],
    recipient_x25519: &[u8; 32],
    sender_ed25519: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(E2E_KEY_DOMAIN);
    hasher.update(shared_secret);
    hasher.update(eph_pub);
    hasher.update(recipient_x25519);
    hasher.update(sender_ed25519);
    let digest = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

fn build_aad(
    version: u8,
    message_id: &str,
    timestamp: i64,
    sender_fp: &str,
    recipient_fp: &str,
) -> Vec<u8> {
    format!(
        "v{version}|{message_id}|{timestamp}|{sender_fp}|{recipient_fp}"
    )
    .into_bytes()
}

fn build_signature_transcript(
    version: u8,
    message_id: &str,
    timestamp: i64,
    sender_fp: &str,
    recipient_fp: &str,
    eph_pub: &[u8],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(ENVELOPE_SIG_DOMAIN);
    hasher.update([version]);
    hasher.update((message_id.len() as u32).to_be_bytes());
    hasher.update(message_id.as_bytes());
    hasher.update(timestamp.to_be_bytes());
    hasher.update((sender_fp.len() as u32).to_be_bytes());
    hasher.update(sender_fp.as_bytes());
    hasher.update((recipient_fp.len() as u32).to_be_bytes());
    hasher.update(recipient_fp.as_bytes());
    hasher.update(eph_pub);
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().to_vec()
}

/// Encrypt and sign a payload from `sender` to `recipient`.
pub fn seal_envelope(
    sender: &Identity,
    recipient: &PeerPublicKey,
    message_id: &str,
    timestamp: i64,
    plaintext: &[u8],
) -> Result<EncryptedEnvelope> {
    let eph_secret = EphemeralSecret::random_from_rng(OsRng);
    let eph_public = X25519PublicKey::from(&eph_secret);
    let recipient_x25519 = recipient.x25519_public();
    let shared = eph_secret.diffie_hellman(&recipient_x25519);
    if !shared.was_contributory() {
        return Err(CryptoError::WeakSharedSecret);
    }

    let mut sym_key = derive_symmetric_key(
        shared.as_bytes(),
        eph_public.as_bytes(),
        recipient_x25519.as_bytes(),
        sender.verifying_key.as_bytes(),
    );

    let mut nonce_bytes = [0u8; 24];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    let aad = build_aad(
        ENVELOPE_VERSION,
        message_id,
        timestamp,
        sender.fingerprint(),
        recipient.fingerprint(),
    );

    let cipher = XChaCha20Poly1305::new((&sym_key).into());
    sym_key.zeroize();
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::AeadFailure)?;

    let transcript = build_signature_transcript(
        ENVELOPE_VERSION,
        message_id,
        timestamp,
        sender.fingerprint(),
        recipient.fingerprint(),
        eph_public.as_bytes(),
        &nonce_bytes,
        &ciphertext,
    );
    let sig = sender.signing_key.sign(&transcript);

    Ok(EncryptedEnvelope {
        version: ENVELOPE_VERSION,
        message_id: message_id.to_string(),
        timestamp,
        sender_fingerprint: sender.fingerprint().to_string(),
        recipient_fingerprint: recipient.fingerprint().to_string(),
        ephemeral_pubkey: B64.encode(eph_public.as_bytes()),
        nonce: B64.encode(nonce_bytes),
        ciphertext: B64.encode(ciphertext),
        signature: B64.encode(sig.to_bytes()),
    })
}

/// Verify Ed25519 signature against the whitelisted `expected_sender` and decrypt for `recipient`.
pub fn open_envelope(
    recipient: &Identity,
    expected_sender: &PeerPublicKey,
    env: &EncryptedEnvelope,
) -> Result<Vec<u8>> {
    if env.version != ENVELOPE_VERSION {
        return Err(CryptoError::UnsupportedVersion(env.version));
    }
    if env.recipient_fingerprint != recipient.fingerprint() {
        return Err(CryptoError::RecipientMismatch);
    }
    if env.sender_fingerprint != expected_sender.fingerprint() {
        return Err(CryptoError::SenderMismatch);
    }

    let eph_bytes: [u8; 32] = B64
        .decode(&env.ephemeral_pubkey)?
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let nonce_bytes: [u8; 24] = B64
        .decode(&env.nonce)?
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let ciphertext = B64.decode(&env.ciphertext)?;
    let sig_bytes: [u8; 64] = B64
        .decode(&env.signature)?
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;

    let transcript = build_signature_transcript(
        env.version,
        &env.message_id,
        env.timestamp,
        &env.sender_fingerprint,
        &env.recipient_fingerprint,
        &eph_bytes,
        &nonce_bytes,
        &ciphertext,
    );

    let sig = Signature::from_bytes(&sig_bytes);
    expected_sender
        .verifying_key
        .verify(&transcript, &sig)
        .map_err(|_| CryptoError::InvalidSignature)?;

    let eph_pub = X25519PublicKey::from(eph_bytes);
    let recipient_secret = recipient.x25519_secret();
    let recipient_x25519 = X25519PublicKey::from(&recipient_secret);
    let shared = recipient_secret.diffie_hellman(&eph_pub);
    if !shared.was_contributory() {
        return Err(CryptoError::WeakSharedSecret);
    }

    let mut sym_key = derive_symmetric_key(
        shared.as_bytes(),
        eph_pub.as_bytes(),
        recipient_x25519.as_bytes(),
        expected_sender.verifying_key.as_bytes(),
    );

    let aad = build_aad(
        env.version,
        &env.message_id,
        env.timestamp,
        &env.sender_fingerprint,
        &env.recipient_fingerprint,
    );

    let cipher = XChaCha20Poly1305::new((&sym_key).into());
    sym_key.zeroize();
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce_bytes),
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::AeadFailure)?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_to_x25519_matches() {
        let id = Identity::generate("flo@test").unwrap();
        let peer = id.to_peer_public_key();
        let from_secret = X25519PublicKey::from(&id.x25519_secret());
        let from_verifying = peer.x25519_public();
        assert_eq!(from_secret.to_bytes(), from_verifying.to_bytes());
    }

    #[test]
    fn openssh_save_load_roundtrip() {
        let id = Identity::generate("benjamin@team").unwrap();
        let dir = std::env::temp_dir().join(format!("pa2pa-crypto-test-{}", rand::random::<u64>()));
        let key_path = dir.join("id_ed25519");
        id.save_openssh(&key_path, "benjamin@team").unwrap();

        let loaded = Identity::load_openssh(&key_path).unwrap();
        assert_eq!(id.fingerprint(), loaded.fingerprint());
        assert_eq!(id.openssh_public_key(), loaded.openssh_public_key());

        let parsed_pub = PeerPublicKey::from_openssh(loaded.openssh_public_key()).unwrap();
        assert_eq!(parsed_pub.fingerprint(), id.fingerprint());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn e2e_seal_open_and_tamper_detection() {
        let flo = Identity::generate("flo").unwrap();
        let benjamin = Identity::generate("benjamin").unwrap();
        let eve = Identity::generate("eve").unwrap();

        let msg = b"Hey Flo's agent, can you check the API schema in mon-app?";
        let env = seal_envelope(&benjamin, &flo.to_peer_public_key(), "msg-1", 1700000000, msg)
            .unwrap();

        let decrypted = open_envelope(&flo, &benjamin.to_peer_public_key(), &env).unwrap();
        assert_eq!(decrypted, msg);

        // Wrong sender rejected
        assert!(open_envelope(&flo, &eve.to_peer_public_key(), &env).is_err());
        // Wrong recipient rejected
        assert!(open_envelope(&eve, &benjamin.to_peer_public_key(), &env).is_err());

        // Tampered ciphertext rejected
        let mut tampered = env.clone();
        let mut ct = B64.decode(&tampered.ciphertext).unwrap();
        ct[0] ^= 0xFF;
        tampered.ciphertext = B64.encode(ct);
        assert!(open_envelope(&flo, &benjamin.to_peer_public_key(), &tampered).is_err());
    }

    #[test]
    fn relay_challenge_roundtrip() {
        let flo = Identity::generate("flo").unwrap();
        let nonce = "random-nonce-12345";
        let sig = flo.sign_relay_challenge(nonce);
        let peer = verify_relay_challenge(flo.openssh_public_key(), nonce, &sig).unwrap();
        assert_eq!(peer.fingerprint(), flo.fingerprint());

        assert!(verify_relay_challenge(flo.openssh_public_key(), "wrong-nonce", &sig).is_err());
    }

    #[test]
    fn local_at_rest_encryption_roundtrip_and_legacy_compat() {
        let flo = Identity::generate("flo").unwrap();
        let eve = Identity::generate("eve").unwrap();
        let secret = "Confidential discussion about API keys";
        let enc = flo.encrypt_local_text(secret).unwrap();
        assert!(enc.starts_with(AT_REST_PREFIX));
        assert_ne!(enc, secret);
        assert_eq!(flo.decrypt_local_text(&enc).unwrap(), secret);
        assert!(eve.decrypt_local_text(&enc).is_err());
        // Legacy plaintext rows still readable
        assert_eq!(flo.decrypt_local_text("plain legacy").unwrap(), "plain legacy");
    }
}
