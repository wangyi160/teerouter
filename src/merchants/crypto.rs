//! Independent merchant-local control credentials; no platform recovery cipher.
use super::*;
use sha2::{Digest, Sha256};

pub(super) fn digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(super) fn public_identity(public: &str) -> Option<String> {
    let key = crate::request_proof::unhex(public, 32).ok()?;
    Some(format!("m_sig_{}", crate::request_proof::hex(&Sha256::digest(key))))
}
#[cfg(test)]
pub(crate) fn identity(token: &str) -> String {
    public_identity(&digest(token.split('.').next().unwrap_or(token))).unwrap()
}
pub(super) fn valid_secret(secret: &str) -> bool {
    secret.len() == 64
        && secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) fn control_token(token: &str) -> Result<(&str, &str), ApiError> {
    let (enrollment, owner) = token.split_once('.').ok_or_else(forbidden)?;
    if !valid_secret(enrollment) || !valid_secret(owner) || enrollment == owner {
        return Err(forbidden());
    }
    Ok((enrollment, owner))
}
