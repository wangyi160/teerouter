use super::*;

/// Keep each attempt's price dimensions and evidence; round only once per request.
/// Unknown attempts remain explicit even when other attempts can be charged.
pub(crate) fn aggregate(attempts: &[Value], expected: usize) -> Value {
    if expected == 0 && attempts.is_empty() {
        return json!({"version":2,"state":"calculated","currency":"USD",
            "reason":"not_forwarded","usage_source":"none","coverage":"complete",
            "charge_microusd":0,"official_cost_microusd":0,"official_cost_picousd":"0",
            "attempt_records":[],"unknown_attempts":0});
    }
    let invalid = |reason| json!({"version":2,"state":"pending","reason":reason});
    if attempts.is_empty() || attempts.len() != expected || attempts.len() > 16 {
        return invalid("attempt_evidence_missing");
    }
    let first = &attempts[0]["billing"];
    let mut out = json!({"version":2,"state":"pending","attempt_records":attempts});
    for field in [
        "calculator",
        "provider",
        "basis",
        "currency",
        "official_price_revision",
        "official_version",
        "multiplier_millionths",
    ] {
        if attempts.iter().any(|a| a["billing"][field] != first[field]) {
            return invalid("attempt_quote_mismatch");
        }
        out[field] = first[field].clone();
    }
    let mut total = 0u128;
    let mut priced = 0;
    let mut estimated = false;
    let mut official = false;
    let mut unknown = 0;
    for (i, a) in attempts.iter().enumerate() {
        if a["number"].as_u64() != Some(i as u64 + 1) {
            return invalid("attempt_evidence_missing");
        }
        let b = &a["billing"];
        if b["state"] != "calculated" {
            unknown += 1;
            continue;
        }
        let Some(amount) = b["official_cost_picousd"]
            .as_str()
            .and_then(|v| v.parse::<u128>().ok())
        else {
            return invalid("invalid_attempt_charge");
        };
        let Some(sum) = total.checked_add(amount) else {
            return invalid("cost_overflow");
        };
        total = sum;
        priced += 1;
        estimated |= b["usage_source"] == "estimated" || b["usage_source"] == "mixed";
        official |= b["usage_source"] == "official" || b["usage_source"] == "mixed";
    }
    out["usage_source"] = if estimated && official {
        "mixed"
    } else if estimated {
        "estimated"
    } else if official {
        "official"
    } else {
        "none"
    }
    .into();
    out["coverage"] = if unknown > 0 || estimated {
        "partial"
    } else {
        "complete"
    }
    .into();
    out["unknown_attempts"] = json!(unknown);
    if priced == 0 {
        out["coverage"] = "unknown".into();
        out["reason"] = "usage_unavailable".into();
        return out;
    }
    let Some(multiplier) = first["multiplier_millionths"].as_u64() else {
        return invalid("invalid_attempt_charge");
    };
    let Some(numerator) = total.checked_mul(multiplier.into()) else {
        return invalid("cost_overflow");
    };
    let (Ok(charge), Ok(official)) = (
        u64::try_from(numerator.div_ceil(1_000_000_000_000)),
        u64::try_from(total.div_ceil(1_000_000)),
    ) else {
        return invalid("cost_overflow");
    };
    out["state"] = "calculated".into();
    out["official_cost_picousd"] = total.to_string().into();
    out["charge_numerator"] = numerator.to_string().into();
    out["charge_denominator"] = "1000000000000".into();
    out["rounding"] = "ceil_once_to_micro_usd".into();
    out["charge_microusd"] = charge.into();
    out["official_cost_microusd"] = official.into();
    out
}

pub(crate) fn tokens(record: &Value) -> crate::upstream::Tokens {
    let mut total = crate::upstream::Tokens::default();
    if let Some(attempts) = record["attempt_records"].as_array() {
        for attempt in attempts {
            let b = &attempt["billing"];
            let u = &b["usage"];
            let read = u["cache_read"].as_u64().unwrap_or(0);
            let write = u["cache_write"].as_u64().unwrap_or(0);
            let mut input = u["input"].as_u64().unwrap_or(0);
            if b["provider"] == "anthropic" {
                input = input.saturating_add(read).saturating_add(write);
            }
            total.input = total.input.saturating_add(input);
            let output = if b["state"] == "calculated" {
                b["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| i["kind"] == "output")
                    .fold(0u64, |n, i| {
                        n.saturating_add(i["quantity"].as_u64().unwrap_or(0))
                    })
            } else {
                u["output"].as_u64().unwrap_or(0)
            };
            total.output = total.output.saturating_add(output);
            total.cache_read = total.cache_read.saturating_add(read);
            total.cache_write = total.cache_write.saturating_add(write);
            total.cache_write_hour = total
                .cache_write_hour
                .saturating_add(u["cache_write_hour"].as_u64().unwrap_or(0));
        }
    }
    total.known = record["coverage"] == "complete";
    total
}
