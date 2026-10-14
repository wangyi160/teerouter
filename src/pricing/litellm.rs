//! Import direct-provider text prices. Money is parsed as decimal, never f64.
use super::*;
use crate::billing::{Rules, Variant};
use serde_json::Map;

fn bad() -> ApiError {
    ApiError::new(
        502,
        "invalid_price_source",
        "Price source has missing, invalid or unsupported token prices; previous prices retained",
    )
}

/// USD/token -> micro USD/million tokens, with no rounding of source prices.
pub(super) fn money(v: &Value) -> Result<u64, ApiError> {
    decimal(v, 1_000_000_000_000)
}
fn decimal(v: &Value, limit: u64) -> Result<u64, ApiError> {
    let s = v.as_number().ok_or_else(bad)?.to_string();
    let (mantissa, exponent) = s.split_once(['e', 'E']).unwrap_or((&s, "0"));
    let exponent: i32 = exponent.parse().map_err(|_| bad())?;
    if mantissa.starts_with('-') || !(-100..=100).contains(&exponent) {
        return Err(bad());
    }
    let decimals = mantissa
        .split_once('.')
        .map_or(0, |(_, tail)| tail.len() as i32);
    let digits = mantissa.replace('.', "");
    let mut n: u128 = digits.parse().map_err(|_| bad())?;
    let power = 12 + exponent - decimals;
    if n == 0 {
        return Ok(0);
    }
    if power >= 0 {
        n = n
            .checked_mul(10u128.checked_pow(power as u32).ok_or_else(bad)?)
            .ok_or_else(bad)?;
    } else {
        let d = 10u128.checked_pow((-power) as u32).ok_or_else(bad)?;
        if n % d != 0 {
            return Err(bad());
        }
        n /= d;
    }
    if n > u128::from(limit) {
        return Err(bad());
    }
    Ok(n as u64)
}

fn rates(
    v: &Map<String, Value>,
    context: &str,
    tier: &str,
) -> Result<Option<TokenRates>, ApiError> {
    let get = |key: &str| {
        v.get(&format!("{key}{context}{tier}"))
            .map(money)
            .transpose()
    };
    let input = get("input_cost_per_token")?;
    let output = get("output_cost_per_token")?;
    if input.is_none() && output.is_none() {
        return Ok(None);
    }
    Ok(Some(TokenRates {
        input: input.ok_or_else(bad)?,
        output: output.ok_or_else(bad)?,
        cache_read: get("cache_read_input_token_cost")?,
        cache_write: get("cache_creation_input_token_cost")?,
        cache_write_hour: get("cache_creation_input_token_cost_above_1hr")?,
    }))
}

fn context(v: &Map<String, Value>) -> Result<Option<(String, u64)>, ApiError> {
    let mut result = None;
    for key in v.keys() {
        let Some(tail) = key.strip_prefix("input_cost_per_token_above_") else {
            continue;
        };
        let Some(number) = tail.strip_suffix("k_tokens") else {
            continue;
        };
        let threshold = number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(1000))
            .filter(|n| *n > 0)
            .ok_or_else(bad)?;
        if result.is_some() {
            return Err(bad());
        }
        result = Some((format!("_above_{tail}"), threshold));
    }
    Ok(result)
}

fn uplift(r: &TokenRates, multiplier: u64) -> Result<TokenRates, ApiError> {
    let mul = |n: u64| {
        let p = u128::from(n) * u128::from(multiplier);
        if p % 1_000_000_000_000 != 0 || p / 1_000_000_000_000 > 1_000_000_000_000 {
            return Err(bad());
        }
        Ok((p / 1_000_000_000_000) as u64)
    };
    Ok(TokenRates {
        input: mul(r.input)?,
        output: mul(r.output)?,
        cache_read: r.cache_read.map(mul).transpose()?,
        cache_write: r.cache_write.map(mul).transpose()?,
        cache_write_hour: r.cache_write_hour.map(mul).transpose()?,
    })
}

pub(super) fn parse(bytes: &[u8], source: &str, now: u64) -> Result<Vec<ProviderBook>, ApiError> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
    let root = root
        .as_object()
        .filter(|v| v.len() <= 100_000)
        .ok_or_else(bad)?;
    let mut providers = vec![];
    for (name, provider) in [
        ("openai", Provider::OpenAI),
        ("anthropic", Provider::Anthropic),
        ("xai", Provider::XAI),
    ] {
        let mut models = std::collections::BTreeMap::new();
        for (id, value) in root {
            if value["litellm_provider"] != name
                || !matches!(value["mode"].as_str(), Some("chat" | "responses"))
            {
                continue;
            }
            let id = id.strip_prefix(&format!("{name}/")).unwrap_or(id);
            // Fine-tuning templates, containers and dedicated audio models aren't routed text models.
            if id.contains('/')
                || id.starts_with("ft:")
                || id == "container"
                || id.contains("audio")
            {
                continue;
            }
            let v = value.as_object().ok_or_else(bad)?;
            let base = rates(v, "", "")?.ok_or_else(bad)?;
            let threshold = context(v)?;
            let long = |tier: &str| -> Result<Option<LongContextPrice>, ApiError> {
                match &threshold {
                    Some((suffix, n)) => Ok(rates(v, suffix, tier)?.map(|r| LongContextPrice {
                        above_input_tokens: *n,
                        rates: r,
                    })),
                    None => Ok(None),
                }
            };
            let long_context = long("")?;
            let mut rules = Rules {
                long_context_inclusive: provider == Provider::XAI && long_context.is_some(),
                ..Default::default()
            };
            for tier in ["priority", "flex"] {
                let suffix = format!("_{tier}");
                if !v.contains_key(&format!("input_cost_per_token{suffix}"))
                    || !v.contains_key(&format!("output_cost_per_token{suffix}"))
                {
                    continue;
                }
                if let Some(rates) = rates(v, "", &suffix)? {
                    let long_context = long(&suffix)?;
                    // A partial tier must not silently use short-context rates for long requests.
                    if threshold.is_some() && long_context.is_none() {
                        continue;
                    }
                    rules.variants.push(Variant {
                        service_tier: tier.into(),
                        speed: "standard".into(),
                        inference_geo: "global".into(),
                        rates,
                        long_context,
                    });
                }
            }
            if provider == Provider::Anthropic {
                // LiteLLM's Anthropic calculator applies geo/speed multipliers to all token costs.
                let entries = &value["provider_specific_entry"];
                let mut seeds = rules.variants.clone();
                seeds.push(Variant {
                    service_tier: "default".into(),
                    speed: "standard".into(),
                    inference_geo: "global".into(),
                    rates: base.clone(),
                    long_context: long_context.clone(),
                });
                for seed in seeds {
                    for (geo, speed) in [("us", "standard"), ("global", "fast"), ("us", "fast")] {
                        if (geo == "us" && entries.get("us").is_none())
                            || (speed == "fast" && entries.get("fast").is_none())
                        {
                            continue;
                        }
                        let mut variant = seed.clone();
                        variant.inference_geo = geo.into();
                        variant.speed = speed.into();
                        for key in [
                            if geo == "us" { Some("us") } else { None },
                            if speed == "fast" { Some("fast") } else { None },
                        ]
                        .into_iter()
                        .flatten()
                        {
                            let m = decimal(&entries[key], 10_000_000_000_000)?;
                            if m == 0 {
                                return Err(bad());
                            }
                            variant.rates = uplift(&variant.rates, m)?;
                            if let Some(long) = &mut variant.long_context {
                                long.rates = uplift(&long.rates, m)?;
                            }
                        }
                        rules.variants.push(variant);
                    }
                }
            }
            let model = ModelPrice {
                model: id.into(),
                rates: base,
                long_context,
                valid_until: None,
                billing_rules: rules,
            };
            if let Some(old) = models.insert(id.to_owned(), model.clone()) {
                if serde_json::to_value(old).unwrap() != serde_json::to_value(model).unwrap() {
                    return Err(bad());
                }
            }
        }
        if models.is_empty() {
            return Err(bad());
        }
        providers.push(ProviderBook {
            provider,
            version: 1,
            source: source.into(),
            checked_at: now,
            models: models.into_values().collect(),
        });
    }
    super::validate(&Book {
        revision: 1,
        providers: providers.clone(),
    })
    .map_err(|_| bad())?;
    Ok(providers)
}
