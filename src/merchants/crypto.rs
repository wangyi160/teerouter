//! Independent merchant-local control credentials; no platform recovery cipher.
use super::*;
use sha2::{Digest, Sha256};

pub(super) fn digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn identity(token: &str) -> String {
    let enrollment = token.split('.').next().unwrap_or(token);
    format!("m_{}", &digest(enrollment)[..32])
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
