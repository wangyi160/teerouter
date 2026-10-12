//! Merchant recovery is encrypted before it reaches the platform checkpoint.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::aead;
use sha2::{Digest, Sha256};

pub(super) fn digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn identity(secret: &str) -> String {
    format!("m_{}", &digest(secret)[..32])
}
pub(super) fn key(secret: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"teerouter:merchant-recovery:v1\0");
    hash.update(secret.as_bytes());
    hash.finalize().into()
}
pub(super) fn seal(id: &str, key: &[u8; 32], state: &Recovery) -> Result<Capsule, ApiError> {
    let mut nonce = [0; 12];
    getrandom::getrandom(&mut nonce).map_err(|_| unavailable())?;
    let cipher = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).map_err(|_| unavailable())?,
    );
    let mut bytes = zeroize::Zeroizing::new(serde_json::to_vec(state).map_err(|_| unavailable())?);
    cipher
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(id.as_bytes()),
            &mut *bytes,
        )
        .map_err(|_| unavailable())?;
    Ok(Capsule {
        version: 1,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(&*bytes),
    })
}
pub(super) fn open(id: &str, key: &[u8; 32], capsule: &Capsule) -> Result<Recovery, ApiError> {
    let error = || {
        ApiError::new(
            409,
            "merchant_recovery_invalid",
            "Merchant backup authentication failed",
        )
    };
    if capsule.version != 1 || capsule.ciphertext.len() > 48 * 1024 * 1024 {
        return Err(error());
    }
    let nonce: [u8; 12] = STANDARD
        .decode(&capsule.nonce)
        .map_err(|_| error())?
        .try_into()
        .map_err(|_| error())?;
    let mut bytes =
        zeroize::Zeroizing::new(STANDARD.decode(&capsule.ciphertext).map_err(|_| error())?);
    let cipher = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).map_err(|_| error())?,
    );
    let plain = cipher
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(id.as_bytes()),
            &mut bytes,
        )
        .map_err(|_| error())?;
    let state: Recovery = serde_json::from_slice(plain).map_err(|_| error())?;
    if state.id != id
        || !state.state.merchants.is_empty()
        || state.state.instance.len() != 32
        || !state.state.instance.bytes().all(|b| b.is_ascii_hexdigit())
        || state.state.revision == 0
        || state.state.generation > u64::MAX / 2
        || state.state.sequence > u64::MAX / 2
        || state.state.events.len() > 8192
    {
        return Err(error());
    }
    Ok(state)
}
