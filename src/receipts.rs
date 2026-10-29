//! Signing identities and envelopes for public relay consumption batches.
use crate::vault::VaultError;
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub(crate) fn unhex(value: &str, size: usize) -> Option<Vec<u8>> {
    if value.len() != size * 2
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    (0..size)
        .map(|i| u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    pub batch_no: u64,
    pub batch_hash: String,
    pub through_seq: u64,
    pub boot_id: String,
    pub created_at_ms: u64,
}
impl Head {
    pub(crate) fn valid(&self, sequence: u64) -> bool {
        self.batch_no > 0
            && self.batch_no < u64::MAX / 2
            && self.through_seq == sequence
            && unhex(&self.batch_hash, 32).is_some()
            && unhex(&self.boot_id, 16).is_some()
    }
}

pub(crate) struct Signer(Ed25519KeyPair);
impl Signer {
    pub(crate) fn new() -> Result<Self, VaultError> {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| VaultError("receipt key unavailable"))?;
        Ok(Self(
            Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
                .map_err(|_| VaultError("receipt key unavailable"))?,
        ))
    }
    pub(crate) fn public_key(&self) -> String {
        hex(self.0.public_key().as_ref())
    }
    pub(crate) fn sign_public(&self, manifest: &str) -> Value {
        let message = [
            b"teerouter:public-consumption-batch:v1\0".as_slice(),
            manifest.as_bytes(),
        ]
        .concat();
        json!({"manifest":manifest,"signature":hex(self.0.sign(&message).as_ref()),
            "batch_hash":hex(&digest(&message))})
    }
    pub(crate) fn identity_binding(&self, boot: &str, nonce: &str) -> Option<[u8; 64]> {
        let mut hash = Sha512::new();
        hash.update(b"teerouter:receipt-key:v1\0");
        hash.update(unhex(nonce, 32)?);
        hash.update(self.0.public_key().as_ref());
        hash.update(unhex(boot, 16)?);
        Some(hash.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_batch_signature_authenticates_manifest_and_record_digest() {
        let signer = Signer::new().unwrap();
        let rows = r#"[{"seq":1,"charge_microusd":"17"}]"#;
        let manifest = json!({"records_sha256":hex(&digest(rows.as_bytes())),"record_count":1}).to_string();
        let batch = signer.sign_public(&manifest);
        let key = unhex(&signer.public_key(),32).unwrap();
        let signature = unhex(batch["signature"].as_str().unwrap(),64).unwrap();
        let domain = b"teerouter:public-consumption-batch:v1\0";
        let message = [domain.as_slice(),manifest.as_bytes()].concat();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519,&key).verify(&message,&signature).unwrap();
        let changed = manifest.replace(&hex(&digest(rows.as_bytes())),&hex(&digest(rows.replace("17","18").as_bytes())));
        assert!(ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519,&key)
            .verify(&[domain.as_slice(),changed.as_bytes()].concat(),&signature).is_err());
    }
}
