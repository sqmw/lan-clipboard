use crate::settings::Settings;
use aes_gcm_siv::{
    aead::{Aead, KeyInit, Payload},
    Aes256GcmSiv, Nonce as AesNonce,
};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

const DISCOVERY_CONTEXT: &[u8] = b"lan-clipboard/discovery/v5";
const SESSION_SALT_CONTEXT: &[u8] = b"lan-clipboard/session-salt/v5";
const SESSION_KEYS_CONTEXT: &[u8] = b"lan-clipboard/session-keys/v5";

pub(super) struct SessionKeyMaterial {
    pub(super) session_id: [u8; 16],
    pub(super) client_to_server_control: [u8; 32],
    pub(super) server_to_client_control: [u8; 32],
    pub(super) client_to_server_raw: [u8; 32],
    pub(super) server_to_client_raw: [u8; 32],
}

pub(super) fn effective_secret(settings: &Settings) -> String {
    settings.sync.shared_code.trim().to_string()
}

pub(super) fn discovery_domain_id(settings: &Settings) -> String {
    discovery_domain_id_from_secret(&effective_secret(settings))
}

pub(super) fn discovery_domain_id_from_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DISCOVERY_CONTEXT);
    hasher.update([0]);
    hasher.update(secret.trim().as_bytes());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(32);
    for byte in &digest[..16] {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

pub(super) fn derive_session_key_material(
    secret: &str,
    transcript: &[u8],
) -> anyhow::Result<SessionKeyMaterial> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(anyhow::anyhow!("shared code is empty"));
    }

    let mut hasher = Sha256::new();
    hasher.update(SESSION_SALT_CONTEXT);
    hasher.update([0]);
    hasher.update(transcript);
    let salt = hasher.finalize();
    let hkdf = Hkdf::<Sha256>::new(Some(salt.as_slice()), secret.as_bytes());
    let mut output = [0u8; 16 + 32 * 4];
    hkdf.expand(SESSION_KEYS_CONTEXT, &mut output)
        .map_err(|_| anyhow::anyhow!("session key derivation failed"))?;

    let mut session_id = [0u8; 16];
    session_id.copy_from_slice(&output[..16]);
    let mut client_to_server_control = [0u8; 32];
    client_to_server_control.copy_from_slice(&output[16..48]);
    let mut server_to_client_control = [0u8; 32];
    server_to_client_control.copy_from_slice(&output[48..80]);
    let mut client_to_server_raw = [0u8; 32];
    client_to_server_raw.copy_from_slice(&output[80..112]);
    let mut server_to_client_raw = [0u8; 32];
    server_to_client_raw.copy_from_slice(&output[112..144]);
    Ok(SessionKeyMaterial {
        session_id,
        client_to_server_control,
        server_to_client_control,
        client_to_server_raw,
        server_to_client_raw,
    })
}

pub(super) fn encrypt_bytes(
    plain: &[u8],
    aad: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, Payload { msg: plain, aad })
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((nonce_bytes, encrypted))
}

pub(super) fn decrypt_bytes(
    nonce_bytes: [u8; 12],
    body: &[u8],
    aad: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, Payload { msg: body, aad })
        .map_err(|_| anyhow::anyhow!("decrypt failed (shared code mismatch?)"))?;
    Ok(plain)
}

pub(super) fn encrypt_raw_payload_bytes(
    plain: &[u8],
    aad: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, Payload { msg: plain, aad })
        .map_err(|_| anyhow::anyhow!("encrypt raw payload failed"))?;
    Ok((nonce_bytes, encrypted))
}

pub(super) fn decrypt_raw_payload_bytes(
    nonce_bytes: [u8; 12],
    body: &[u8],
    aad: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, Payload { msg: body, aad })
        .map_err(|_| anyhow::anyhow!("decrypt raw payload failed (shared code mismatch?)"))?;
    Ok(plain)
}
