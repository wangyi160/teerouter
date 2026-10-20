use crate::protocol::hex;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
const DOMAIN: &[u8] = b"teerouter:consumption-batch:v1\0";
fn bytes(value: &Value, size: usize) -> Result<Vec<u8>> {
    let s = value.as_str().context("missing hex string")?;
    ensure!(
        s.len() == size * 2
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid lowercase hex"
    );
    (0..size)
        .map(|i| Ok(u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?))
        .collect()
}
fn number(v: &Value) -> Result<u64> {
    v.as_u64().context("invalid integer")
}
fn leaf(record: &Value) -> Result<[u8; 32]> {
    let payload = record["payload"]
        .as_str()
        .context("missing record payload")?;
    ensure!(payload.len() <= 2 * 1024 * 1024, "record too large");
    let mut h = Sha256::new();
    h.update([0]);
    h.update(bytes(&record["salt"], 32)?);
    h.update(payload.as_bytes());
    Ok(h.finalize().into())
}
fn parent(left: &[u8], right: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([1]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}
fn root(mut leaves: Vec<[u8; 32]>) -> [u8; 32] {
    if leaves.is_empty() {
        return Sha256::digest(b"").into();
    }
    while leaves.len() > 1 {
        leaves = leaves
            .chunks(2)
            .map(|p| parent(&p[0], p.get(1).unwrap_or(&p[0])))
            .collect();
    }
    leaves[0]
}
pub fn verify_batch(batch: &Value) -> Result<(Value, usize)> {
    let payload = batch["manifest"]
        .as_str()
        .context("missing signed manifest")?;
    ensure!(payload.len() <= 16384, "manifest too large");
    let m: Value = serde_json::from_str(payload)?;
    if m["schema"] == "teerouter.public-consumption-batch.v1" {
        return verify_public_batch(batch, &m, payload);
    }
    ensure!(
        m["schema"] == "teerouter.consumption-batch.v1"
            && m["scope"] == "router_consumption_only"
            && m["algorithm"] == "ed25519"
            && m["currency"] == "USD",
        "unsupported batch"
    );
    let key = bytes(&m["public_key"], 32)?;
    bytes(&m["boot_id"], 16)?;
    bytes(&m["instance_id"], 16)?;
    let signature = bytes(&batch["signature"], 64)?;
    let message = [DOMAIN, payload.as_bytes()].concat();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&message, &signature)
        .map_err(|_| anyhow::anyhow!("invalid batch signature"))?;
    ensure!(
        hex(&Sha256::digest(&message)) == batch["batch_hash"],
        "batch hash mismatch"
    );
    let before = number(&m["sequence_before"])?;
    let through = number(&m["sequence_through"])?;
    let count = number(&m["record_count"])?;
    ensure!(
        count <= 8192 && through.checked_sub(before) == Some(count) && number(&m["batch_no"])? > 0,
        "invalid batch sequence range"
    );
    let batch_no = number(&m["batch_no"])?;
    if batch_no == 1 {
        ensure!(
            m["previous_batch_hash"].is_null()
                && m["previous_boot_id"].is_null()
                && m["continuity"] == "start",
            "invalid first batch"
        );
    } else {
        bytes(&m["previous_batch_hash"], 32)?;
        bytes(&m["previous_boot_id"], 16)?;
        ensure!(
            m["continuity"]
                == if m["previous_boot_id"] == m["boot_id"] {
                    "continuous"
                } else {
                    "restart"
                },
            "invalid restart marker"
        );
    }
    number(&m["created_at_ms"])?;
    let priced = number(&m["priced_record_count"])?;
    ensure!(priced <= count, "invalid priced record count");
    let total = m["charge_microusd"]
        .as_str()
        .context("missing charge")?
        .parse::<u128>()?;
    bytes(&m["merkle_root"], 32)?;
    if let Some(records) = batch["records"].as_array() {
        ensure!(records.len() as u64 == count, "record count mismatch");
        let mut leaves = Vec::new();
        let mut sum = 0u128;
        let mut priced_records = 0;
        for (i, record) in records.iter().enumerate() {
            leaves.push(leaf(record)?);
            let row: Value = serde_json::from_str(record["payload"].as_str().unwrap())?;
            ensure!(
                number(&row["seq"])? == before + 1 + i as u64 && row["event"].is_object(),
                "record order mismatch"
            );
            let bill = &row["event"]["billing"];
            if bill["state"] == "calculated"
                && let Some(amount) = bill["charge_microusd"].as_u64()
            {
                sum += u128::from(amount);
                priced_records += 1;
            }
        }
        ensure!(
            hex(&root(leaves)) == m["merkle_root"],
            "Merkle root mismatch"
        );
        ensure!(
            sum == total && priced_records == priced,
            "charge summary mismatch"
        );
        return Ok((m, records.len()));
    }
    ensure!(
        !batch
            .as_object()
            .context("invalid batch")?
            .contains_key("records"),
        "records must be an array or omitted"
    );
    if count == 0 {
        ensure!(
            hex(&root(vec![])) == m["merkle_root"] && total == 0 && priced == 0,
            "invalid empty batch"
        );
    }
    Ok((m, 0))
}
pub fn verify_inclusion(manifest: &Value, record: &Value) -> Result<()> {
    let mut width = number(&manifest["record_count"])?;
    let mut index = number(&record["index"])?;
    ensure!(width > 0 && index < width, "invalid inclusion index");
    let row: Value = serde_json::from_str(record["payload"].as_str().context("missing payload")?)?;
    ensure!(
        number(&row["seq"])? == number(&manifest["sequence_before"])? + 1 + index,
        "inclusion sequence mismatch"
    );
    let mut hash = leaf(record)?;
    let siblings = record["siblings"]
        .as_array()
        .context("missing Merkle siblings")?;
    ensure!(siblings.len() <= 13, "too many Merkle siblings");
    for sibling in siblings {
        ensure!(width > 1, "excess Merkle siblings");
        let sibling = bytes(sibling, 32)?;
        if index % 2 == 0 {
            if index + 1 >= width {
                ensure!(sibling == hash, "invalid odd leaf duplication");
            }
            hash = parent(&hash, &sibling);
        } else {
            hash = parent(&sibling, &hash);
        }
        index /= 2;
        width = width.div_ceil(2);
    }
    ensure!(
        width == 1 && hex(&hash) == manifest["merkle_root"],
        "inclusion proof mismatch"
    );
    Ok(())
}
pub fn verify_link(previous: &Value, hash: &Value, next: &Value) -> Result<()> {
    ensure!(
        next["previous_batch_hash"] == *hash
            && number(&next["batch_no"])? == number(&previous["batch_no"])? + 1
            && next["sequence_before"] == previous["sequence_through"]
            && next["instance_id"] == previous["instance_id"]
            && (next["schema"] == "teerouter.public-consumption-batch.v1"
                || next["previous_boot_id"] == previous["boot_id"]),
        "batch link mismatch"
    );
    Ok(())
}
fn verify_public_batch(batch: &Value, m: &Value, payload: &str) -> Result<(Value, usize)> {
    ensure!(
        m["algorithm"] == "ed25519" && m["currency"] == "USD",
        "unsupported public batch"
    );
    let key = bytes(&m["public_key"], 32)?;
    for field in ["boot_id", "instance_id"] {
        bytes(&m[field], 16)?;
    }
    for field in ["batch_nonce", "records_sha256"] {
        bytes(&m[field], 32)?;
    }
    let message = [
        b"teerouter:public-consumption-batch:v1\0".as_slice(),
        payload.as_bytes(),
    ]
    .concat();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&message, &bytes(&batch["signature"], 64)?)
        .map_err(|_| anyhow::anyhow!("invalid public batch signature"))?;
    ensure!(
        hex(&Sha256::digest(&message)) == batch["batch_hash"],
        "batch hash mismatch"
    );
    let before = number(&m["sequence_before"])?;
    let through = number(&m["sequence_through"])?;
    let count = number(&m["record_count"])?;
    let number = number(&m["batch_no"])?;
    ensure!(
        number > 0
            && through <= 9_007_199_254_740_991
            && count <= 512
            && through.checked_sub(before) == Some(count),
        "invalid public sequence"
    );
    if number == 1 {
        ensure!(m["previous_batch_hash"].is_null(), "invalid genesis");
    } else {
        bytes(&m["previous_batch_hash"], 32)?;
    }
    let total = m["charge_microusd"]
        .as_str()
        .context("missing total")?
        .parse::<u128>()?;
    if let Some(serialized) = batch.get("records_json") {
        let serialized = serialized
            .as_str()
            .context("records_json must be string or omitted")?;
        ensure!(serialized.len() <= 1024 * 1024, "public records too large");
        ensure!(
            hex(&Sha256::digest(serialized.as_bytes())) == m["records_sha256"],
            "records hash mismatch"
        );
        let rows: Vec<Value> = serde_json::from_str(serialized)?;
        ensure!(rows.len() as u64 == count, "record count mismatch");
        let mut sum = 0u128;
        let fields = [
            "seq",
            "user_hash",
            "key_hash",
            "merchant_id",
            "pool_id",
            "provider",
            "model",
            "status",
            "attempts",
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "charge_microusd",
            "billing_state",
            "refusal_type",
            "created_at",
            "started_at_ms",
            "duration_ms",
            "usage",
            "cache_usage",
            "merchant_price_version",
            "merchant_pricing",
            "billing",
        ];
        for (i, row) in rows.iter().enumerate() {
            ensure!(
                row.as_object()
                    .context("invalid public record")?
                    .keys()
                    .all(|k| fields.contains(&k.as_str())),
                "unknown public field"
            );
            ensure!(
                row["seq"].as_u64() == Some(before + i as u64 + 1),
                "record sequence mismatch"
            );
            bytes(&row["user_hash"], 32)?;
            bytes(&row["key_hash"], 32)?;
            ensure!(
                row["refusal_type"].is_null()
                    || matches!(
                        row["refusal_type"].as_str(),
                        Some("refusal" | "content_filter" | "safety" | "policy_violation")
                    ),
                "invalid refusal type"
            );
            let charge = row["charge_microusd"]
                .as_str()
                .context("missing record charge")?
                .parse::<u128>()?;
            ensure!(
                row["billing_state"] == "calculated" || charge == 0,
                "unpriced nonzero charge"
            );
            sum = sum.checked_add(charge).context("charge overflow")?;
        }
        ensure!(sum == total, "charge total mismatch");
        return Ok((m.clone(), rows.len()));
    }
    Ok((m.clone(), 0))
}
pub fn identity_binding(identity: &Value, expected_nonce: Option<&str>) -> Result<[u8; 64]> {
    ensure!(
        identity["schema"] == "teerouter.receipt-key.v1"
            && identity["evidence_type"] == "dstack_tdx",
        "TDX receipt key evidence required"
    );
    if let Some(nonce) = expected_nonce {
        ensure!(identity["nonce"] == nonce, "identity challenge mismatch");
    }
    let mut h = Sha512::new();
    h.update(b"teerouter:receipt-key:v1\0");
    h.update(bytes(&identity["nonce"], 32)?);
    h.update(bytes(&identity["public_key"], 32)?);
    h.update(bytes(&identity["boot_id"], 16)?);
    Ok(h.finalize().into())
}

#[cfg(test)]
mod tests;
