use crate::settings::Settings;
use aes_gcm_siv::{Aes256GcmSiv, Nonce as AesNonce};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub(super) fn effective_secret(settings: &Settings) -> String {
    settings.sync.shared_code.trim().to_string()
}

pub(super) fn derive_key(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out[..32]);
    key
}

pub(super) fn encrypt_bytes(plain: &[u8], key: &[u8; 32]) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt failed"))?;
    Ok((nonce_bytes, encrypted))
}

pub(super) fn decrypt_bytes(
    nonce_bytes: [u8; 12],
    body: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256GcmSiv::new_from_slice(key)?;
    let nonce = AesNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body)
        .map_err(|_| anyhow::anyhow!("decrypt failed (shared code mismatch?)"))?;
    Ok(plain)
}

pub(super) fn encrypt_raw_payload_bytes(
    plain: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<([u8; 12], Vec<u8>)> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, plain)
        .map_err(|_| anyhow::anyhow!("encrypt raw payload failed"))?;
    Ok((nonce_bytes, encrypted))
}

pub(super) fn decrypt_raw_payload_bytes(
    nonce_bytes: [u8; 12],
    body: &[u8],
    key: &[u8; 32],
) -> anyhow::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new_from_slice(key)?;
    let nonce = ChaChaNonce::from_slice(&nonce_bytes);
    let plain = cipher
        .decrypt(nonce, body)
        .map_err(|_| anyhow::anyhow!("decrypt raw payload failed (shared code mismatch?)"))?;
    Ok(plain)
}
