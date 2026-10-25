use super::*;
use crate::receipts::{digest, hex, unhex};

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn select(value: &Value, fields: &[&str]) -> Value {
    let mut out = serde_json::Map::new();
    for &field in fields {
        if let Some(v) = value.get(field) {
            out.insert(field.into(), v.clone());
        }
    }
    Value::Object(out)
}
fn billing(value: &Value) -> Value {
    billing_detail(value, true)
}
fn billing_detail(value: &Value, attempts: bool) -> Value {
    if !value.is_object() {
        return Value::Null;
    }
    let mut out = select(
        value,
        &[
            "version",
            "calculator",
            "provider",
            "state",
            "basis",
            "currency",
            "official_price_revision",
            "official_version",
            "multiplier_millionths",
            "charge_microusd",
            "official_cost_microusd",
            "official_cost_picousd",
            "charge_numerator",
            "charge_denominator",
            "rounding",
            "reason",
            "usage_source",
            "coverage",
            "upstream_status",
            "upstream_failed",
            "unknown_attempts",
            "termination",
        ],
    );
    if value["transport"].is_object() {
        out["transport"] = select(
            &value["transport"],
            &["version", "phase", "request_flushed"],
        );
    }
    if value["items"].is_array() {
        out["items"] = value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                select(
                    i,
                    &[
                        "kind",
                        "quantity",
                        "unit_price_microusd",
                        "unit",
                        "amount_picousd",
                    ],
                )
            })
            .collect();
    }
    if value["dimensions"].is_object() {
        out["dimensions"] = select(
            &value["dimensions"],
            &["service_tier", "speed", "inference_geo"],
        );
    }
    for field in ["usage", "reported_usage", "waived_usage"] {
        if value[field].is_object() {
            out[field] = select(
                &value[field],
                &[
                    "input",
                    "output",
                    "total",
                    "cache_read",
                    "cache_write",
                    "cache_write_hour",
                    "reasoning",
                    "reported_cost_in_usd_ticks",
                ],
            );
        }
    }
    if value["estimate"].is_object() {
        out["estimate"] = select(
            &value["estimate"],
            &[
                "method",
                "input_tokens",
                "visible_output_tokens",
                "input_source",
                "output_source",
                "cache_policy",
                "context_incomplete",
                "hidden_output_included",
            ],
        );
    }
    if attempts && let Some(records) = value["attempt_records"].as_array() {
        out["attempt_records"] = records
            .iter()
            .take(16)
            .map(|r| {
                json!({"number":r["number"],
            "status":r["status"],"billing":billing_detail(&r["billing"], false)})
            })
            .collect();
    }
    out
}
fn identifier(v: &Value) -> Value {
    v.as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 192
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.:/-".contains(&c))
        })
        .map_or(Value::Null, |s| s.into())
}
fn record(row: &Value, state: &State, nonce: &[u8]) -> Result<Value> {
    let e = &row["event"];
    let user = e["user_id"]
        .as_str()
        .ok_or(VaultError("receipt has no user"))?;
    let private = state
        .config
        .user_ids
        .get(user)
        .and_then(|s| unhex(s, 32))
        .ok_or(VaultError("private receipt identity missing"))?;
    let key = e["key"].as_str().ok_or(VaultError("receipt has no key"))?;
    let user_hash = hex(&digest(&[private.as_slice(), nonce].concat()));
    let key_hash = hex(&digest(
        &[
            b"teerouter:receipt-key-id:v1\0".as_slice(),
            private.as_slice(),
            key.as_bytes(),
            &[0],
            nonce,
        ]
        .concat(),
    ));
    let charged = e["billing"]["state"] == "calculated";
    let charge = if charged {
        e["billing"]["charge_microusd"]
            .as_u64()
            .ok_or(VaultError("invalid charge"))?
    } else {
        0
    };
    let provider = match e["provider"].as_str() {
        Some("openai") => Some("chatgpt"),
        Some("anthropic") => Some("claude"),
        Some("xai") => Some("grok"),
        Some("workbuddy") => Some("workbuddy"),
        _ => None,
    };
    Ok(json!({
        "seq":row["seq"],"user_hash":user_hash,"key_hash":key_hash,
        "merchant_id":identifier(&e["merchant_id"]),"pool_id":identifier(&e["pool"]),
        "provider":provider,"model":identifier(&e["model"]),"status":e["status"],"attempts":e["attempts"],
        "input_tokens":e["usage"]["input_tokens"].as_u64().unwrap_or(0),
        "output_tokens":e["usage"]["output_tokens"].as_u64().unwrap_or(0),
        "cached_input_tokens":e["cache_usage"]["read"].as_u64().unwrap_or(0),
        "charge_microusd":charge.to_string(),
        "billing_state":if charged {"calculated"} else if e["attempts"] == 0 {"not_chargeable"} else {"unpriced"},
        "refusal_type":e["refusal_type"],
        "created_at":e["created_at"],"started_at_ms":e["started_at_ms"],"duration_ms":e["duration_ms"],
        "usage":select(&e["usage"], &["requests","errors","input_tokens","output_tokens","unknown_usage_requests"]),
        "cache_usage":select(&e["cache_usage"], &["read","write","write_hour"]),
        "merchant_price_version":e["merchant_price_version"],
        "merchant_pricing":select(&e["merchant_pricing"], &["basis","official_price_revision","official_version","multiplier_millionths"]),
        "billing":billing(&e["billing"])
    }))
}
pub(super) fn seal(signer: &crate::receipts::Signer, data: &Data, boot: &str) -> Result<Export> {
    let state = data
        .receipt_relay
        .as_ref()
        .ok_or(VaultError("relay not configured"))?;
    let before = state
        .uploaded
        .as_ref()
        .map_or(state.start_sequence, |h| h.through_seq);
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| VaultError("random source unavailable"))?;
    let mut records = vec![];
    let mut consumption = BTreeMap::new();
    let mut refusal_through = 0;
    let mut total = 0u128;
    let mut bytes = 0;
    for row in data.events.iter().take(512) {
        if row["seq"].as_u64() != Some(before + records.len() as u64 + 1) {
            return Err(VaultError("public receipt sequence gap"));
        }
        let public = record(row, state, &nonce)?;
        // Account for escaping the records JSON inside the outer JSON envelope.
        let size = serde_json::to_vec(&serde_json::to_string(&public).unwrap())
            .unwrap()
            .len();
        if size > 900_000 {
            return Err(VaultError("public receipt too large"));
        }
        if bytes + size > 900_000 {
            break;
        }
        bytes += size;
        total += public["charge_microusd"]
            .as_str()
            .unwrap()
            .parse::<u128>()
            .unwrap();
        crate::balances::add(&mut consumption, &row["event"]);
        refusal_through = refusal_through.max(row["event"]["refusal_id"].as_u64().unwrap_or(0));
        records.push(public);
    }
    if records.is_empty() {
        return Err(VaultError("no public receipts"));
    }
    let serialized = serde_json::to_string(&records).unwrap();
    let through = before + records.len() as u64;
    let created = now_ms();
    let number = state.uploaded.as_ref().map_or(1, |h| h.batch_no + 1);
    let manifest = json!({"schema":"teerouter.public-consumption-batch.v1","algorithm":"ed25519",
        "instance_id":data.instance,"boot_id":boot,"public_key":signer.public_key(),"batch_no":number,
        "previous_batch_hash":state.uploaded.as_ref().map(|h| &h.batch_hash),
        "created_at_ms":created,"sequence_before":before,"sequence_through":through,
        "batch_nonce":hex(&nonce),"record_count":records.len(),"records_sha256":hex(&digest(serialized.as_bytes())),
        "charge_microusd":total.to_string(),"currency":"USD"});
    let mut body = signer.sign_public(&serde_json::to_string(&manifest).unwrap());
    let head = Head {
        batch_no: number,
        batch_hash: body["batch_hash"].as_str().unwrap().into(),
        through_seq: through,
        boot_id: boot.into(),
        created_at_ms: created,
    };
    body["records_json"] = serialized.into();
    Ok(Export {
        body,
        head,
        consumption,
        refusal_through,
    })
}
