//! Independently verifiable consumption batches. Keys and unacknowledged exports
//! live only in this process; payment operations are deliberately out of scope.
use crate::vault::{Data, VaultError};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};

pub(crate) const DOMAIN: &[u8] = b"teerouter:consumption-batch:v1\0";
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
    pub(crate) fn identity_binding(&self, boot: &str, nonce: &str) -> Option<[u8; 64]> {
        let mut hash = Sha512::new();
        hash.update(b"teerouter:receipt-key:v1\0");
        hash.update(unhex(nonce, 32)?);
        hash.update(self.0.public_key().as_ref());
        hash.update(unhex(boot, 16)?);
        Some(hash.finalize().into())
    }
    pub(crate) fn seal(&self, data: &Data, boot: &str) -> Result<Export, VaultError> {
        let previous = data.receipt_head.as_ref();
        let before = previous.map(|h| h.through_seq).unwrap_or_else(|| {
            data.events
                .front()
                .and_then(|e| e["seq"].as_u64())
                .map(|s| s.saturating_sub(1))
                .unwrap_or(data.sequence)
        });
        let mut expected = before;
        let mut leaves = Vec::new();
        let mut records = Vec::new();
        let mut total = 0u128;
        let mut priced = 0u64;
        for row in &data.events {
            expected = expected
                .checked_add(1)
                .ok_or(VaultError("receipt sequence overflow"))?;
            if row["seq"].as_u64() != Some(expected) {
                return Err(VaultError("receipt sequence gap"));
            }
            let payload = serde_json::to_string(row).map_err(|_| VaultError("invalid receipt"))?;
            let mut salt = [0u8; 32];
            getrandom::getrandom(&mut salt).map_err(|_| VaultError("random source unavailable"))?;
            let mut leaf = Sha256::new();
            leaf.update([0]);
            leaf.update(salt);
            leaf.update(payload.as_bytes());
            leaves.push(leaf.finalize().into());
            records.push(json!({"payload":payload,"salt":hex(&salt)}));
            let billing = &row["event"]["billing"];
            if billing["state"] == "calculated"
                && let Some(charge) = billing["charge_microusd"].as_u64()
            {
                total += u128::from(charge);
                priced += 1;
            }
        }
        if expected != data.sequence {
            return Err(VaultError("receipt sequence gap"));
        }
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| VaultError("clock unavailable"))?
            .as_millis() as u64;
        let manifest = json!({
            "schema":"teerouter.consumption-batch.v1", "scope":"router_consumption_only",
            "algorithm":"ed25519", "public_key":self.public_key(),
            "instance_id":data.instance, "boot_id":boot,
            "batch_no":previous.map_or(1, |h| h.batch_no + 1),
            "previous_batch_hash":previous.map(|h| &h.batch_hash),
            "previous_boot_id":previous.map(|h| &h.boot_id),
            "continuity":match previous { None=>"start", Some(h) if h.boot_id!=boot=>"restart", _=>"continuous" },
            "created_at_ms":created, "sequence_before":before, "sequence_through":data.sequence,
            "record_count":records.len(), "priced_record_count":priced,
            "charge_microusd":total.to_string(), "currency":"USD",
            "merkle_root":hex(&merkle_root(leaves))
        });
        let payload = serde_json::to_string(&manifest).map_err(|_| VaultError("invalid batch"))?;
        let message = [DOMAIN, payload.as_bytes()].concat();
        let batch_hash = hex(&digest(&message));
        let head = Head {
            batch_no: manifest["batch_no"].as_u64().unwrap(),
            batch_hash: batch_hash.clone(),
            through_seq: data.sequence,
            boot_id: boot.into(),
            created_at_ms: created,
        };
        Ok(Export {
            state: serde_json::to_value(data).map_err(|_| VaultError("invalid snapshot"))?,
            head,
            batch: json!({"manifest":payload,"signature":hex(self.0.sign(&message).as_ref()),"batch_hash":batch_hash,"records":records}),
        })
    }
}

pub(crate) fn merkle_root(mut hashes: Vec<[u8; 32]>) -> [u8; 32] {
    if hashes.is_empty() {
        return digest(b"");
    }
    while hashes.len() > 1 {
        hashes = hashes
            .chunks(2)
            .map(|pair| {
                let mut hash = Sha256::new();
                hash.update([1]);
                hash.update(pair[0]);
                hash.update(*pair.get(1).unwrap_or(&pair[0]));
                hash.finalize().into()
            })
            .collect();
    }
    hashes[0]
}

pub(crate) struct Export {
    pub state: Value,
    pub batch: Value,
    pub head: Head,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signature_commits_to_manifest_and_salted_records() {
        let signer = Signer::new().unwrap();
        let mut data = Data::default();
        data.sequence = 1;
        data.events.push_back(
            json!({"seq":1,"event":{"billing":{"state":"calculated","charge_microusd":17}}}),
        );
        let export = signer.seal(&data, &"a".repeat(32)).unwrap();
        let manifest: Value =
            serde_json::from_str(export.batch["manifest"].as_str().unwrap()).unwrap();
        assert_eq!(manifest["charge_microusd"], "17");
        let key = unhex(&signer.public_key(), 32).unwrap();
        let sig = unhex(export.batch["signature"].as_str().unwrap(), 64).unwrap();
        let message = [
            DOMAIN,
            export.batch["manifest"].as_str().unwrap().as_bytes(),
        ]
        .concat();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
            .verify(&message, &sig)
            .unwrap();
        let mut tampered = message;
        tampered.push(b' ');
        assert!(
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &key)
                .verify(&tampered, &sig)
                .is_err()
        );
        assert_ne!(
            export.batch["records"],
            signer.seal(&data, &"a".repeat(32)).unwrap().batch["records"]
        );
    }
}
