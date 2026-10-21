//! Provider-specific, deterministic request charges. No request/response text is retained.
mod anthropic;
mod estimate;
#[cfg(test)]
mod interrupt_tests;
mod observation;
mod openai;
mod settlement;
#[cfg(test)]
pub(crate) mod tests;
mod xai;

use crate::{
    Provider,
    error::ApiError,
    merchants::{LongContextPrice, TokenRates},
    pricing::Quote,
};
pub(crate) use observation::Observer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub(crate) use settlement::aggregate;
pub(crate) use settlement::tokens;
use std::collections::BTreeMap;

/// Public list-price variants, never merchant-specific discounts or upstream account costs.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Rules {
    pub variants: Vec<Variant>,
    /// Micro-USD per invocation, only for the provider's explicitly supported tools.
    pub tools: BTreeMap<String, u64>,
    pub long_context_inclusive: bool,
}

impl Rules {
    pub(crate) fn is_empty(&self) -> bool {
        self.variants.is_empty() && self.tools.is_empty() && !self.long_context_inclusive
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Variant {
    pub service_tier: String,
    pub speed: String,
    pub inference_geo: String,
    pub rates: TokenRates,
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
}

pub(crate) fn validate(provider: Provider, rules: &Rules) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    rules.variants.len() <= 32
        && rules.tools.len() <= 8
        && (!rules.long_context_inclusive || provider == Provider::XAI)
        && rules
            .tools
            .iter()
            .all(|(k, v)| supported_tool(provider, k) && *v <= 1_000_000_000_000)
        && rules.variants.iter().all(|v| {
            matches!(
                v.service_tier.as_str(),
                "default" | "priority" | "flex" | "scale"
            ) && matches!(v.speed.as_str(), "standard" | "fast")
                && matches!(v.inference_geo.as_str(), "global" | "us")
                && (provider == Provider::Anthropic
                    || (v.speed == "standard" && v.inference_geo == "global"))
                && (v.service_tier != "default"
                    || v.speed != "standard"
                    || v.inference_geo != "global")
                && seen.insert((&v.service_tier, &v.speed, &v.inference_geo))
                && valid_rates(&v.rates)
                && v.long_context.as_ref().is_none_or(|t| {
                    t.above_input_tokens > 0
                        && t.above_input_tokens <= 1_000_000_000
                        && valid_rates(&t.rates)
                })
        })
}

fn valid_rates(r: &TokenRates) -> bool {
    [
        Some(r.input),
        Some(r.output),
        r.cache_read,
        r.cache_write,
        r.cache_write_hour,
    ]
    .into_iter()
    .flatten()
    .all(|v| v <= 1_000_000_000_000)
}

fn supported_tool(provider: Provider, tool: &str) -> bool {
    match provider {
        Provider::Anthropic => tool == "web_search",
        Provider::XAI => matches!(tool, "web_search" | "x_search" | "code_execution"),
        // OpenAI hosted tools have additional context/storage/session charges;
        // they must not be represented by a guessed flat invocation rate.
        _ => false,
    }
}

#[derive(Clone, Default, Serialize)]
pub(crate) struct Metrics {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_write_hour: Option<u64>,
    pub reasoning: Option<u64>,
    pub tools: BTreeMap<String, u64>,
    pub reported_cost_in_usd_ticks: Option<u64>,
}

#[derive(Clone, Serialize)]
pub(crate) struct Dimensions {
    pub service_tier: String,
    pub speed: String,
    pub inference_geo: String,
}

type Checked<T> = Result<T, &'static str>;

#[derive(Serialize)]
struct Line {
    kind: &'static str,
    quantity: u64,
    /// Per-million-token rate, or per-call rate; both in micro-USD.
    unit_price_microusd: u64,
    unit: &'static str,
    /// 10^-12 USD, represented as a decimal integer string for JSON precision.
    amount_picousd: String,
}

#[derive(Default)]
pub(crate) struct Cost {
    total: u128,
    lines: Vec<Line>,
}

impl Cost {
    fn tokens(&mut self, kind: &'static str, quantity: u64, rate: Option<u64>) -> Checked<()> {
        if quantity == 0 {
            return Ok(());
        }
        let rate = rate.ok_or("token_price_missing")?;
        self.line(kind, quantity, rate, false)
    }
    fn line(&mut self, kind: &'static str, quantity: u64, rate: u64, call: bool) -> Checked<()> {
        let amount = u128::from(quantity)
            .checked_mul(u128::from(rate))
            .and_then(|n| n.checked_mul(if call { 1_000_000 } else { 1 }))
            .ok_or("cost_overflow")?;
        self.total = self.total.checked_add(amount).ok_or("cost_overflow")?;
        self.lines.push(Line {
            kind,
            quantity,
            unit_price_microusd: rate,
            unit: if call { "call" } else { "million_tokens" },
            amount_picousd: amount.to_string(),
        });
        Ok(())
    }
    fn tools(&mut self, provider: Provider, usage: &Metrics, rules: &Rules) -> Checked<()> {
        for (tool, count) in &usage.tools {
            if *count == 0 {
                continue;
            }
            if !supported_tool(provider, tool) {
                return Err("unsupported_tool_usage");
            }
            let kind = match tool.as_str() {
                "web_search" => "web_search",
                "x_search" => "x_search",
                "code_execution" => "code_execution",
                _ => unreachable!(),
            };
            self.line(
                kind,
                *count,
                *rules.tools.get(tool).ok_or("tool_price_missing")?,
                true,
            )?;
        }
        Ok(())
    }
}

pub(crate) fn routing_rates(quote: &Quote, body: &Value, input: u64) -> Option<TokenRates> {
    let tier = match body["service_tier"].as_str().unwrap_or("default") {
        "default" | "standard" | "standard_only" | "auto" => "default",
        "priority" | "fast" => "priority",
        "flex" => "flex",
        "scale" => "scale",
        _ => return None,
    };
    rates(
        quote,
        &Dimensions {
            service_tier: tier.into(),
            speed: body["speed"].as_str().unwrap_or("standard").into(),
            inference_geo: body["inference_geo"].as_str().unwrap_or("global").into(),
        },
        input,
    )
    .ok()
}

fn rates(quote: &Quote, dimensions: &Dimensions, total_input: u64) -> Checked<TokenRates> {
    let (base, long) = if dimensions.service_tier == "default"
        && dimensions.speed == "standard"
        && dimensions.inference_geo == "global"
    {
        (
            TokenRates {
                input: quote.price.input,
                output: quote.price.output,
                cache_read: quote.price.cache_read,
                cache_write: quote.price.cache_write,
                cache_write_hour: quote.price.cache_write_hour,
            },
            &quote.price.long_context,
        )
    } else {
        let variant = quote
            .billing_rules
            .variants
            .iter()
            .find(|v| {
                v.service_tier == dimensions.service_tier
                    && v.speed == dimensions.speed
                    && v.inference_geo == dimensions.inference_geo
            })
            .ok_or("price_variant_missing")?;
        (variant.rates.clone(), &variant.long_context)
    };
    Ok(long
        .as_ref()
        .filter(|t| {
            if quote.billing_rules.long_context_inclusive {
                total_input >= t.above_input_tokens
            } else {
                total_input > t.above_input_tokens
            }
        })
        .map(|t| t.rates.clone())
        .unwrap_or(base))
}

fn normalized_cost(
    usage: &Metrics,
    rates: &TokenRates,
    input_includes_cache: bool,
) -> Checked<Cost> {
    let input = usage.input.ok_or("input_usage_missing")?;
    let output = usage.output.ok_or("output_usage_missing")?;
    let read = usage.cache_read.unwrap_or(0);
    let write = usage.cache_write.unwrap_or(0);
    let hour = usage.cache_write_hour.unwrap_or(0);
    let short = write.checked_sub(hour).ok_or("inconsistent_cache_usage")?;
    let plain = if input_includes_cache {
        input
            .checked_sub(read)
            .and_then(|n| n.checked_sub(write))
            .ok_or("inconsistent_cache_usage")?
    } else {
        input
    };
    let mut cost = Cost::default();
    cost.tokens("input", plain, Some(rates.input))?;
    cost.tokens("output", output, Some(rates.output))?;
    cost.tokens("cache_read", read, rates.cache_read)?;
    cost.tokens("cache_write_5m", short, rates.cache_write)?;
    cost.tokens("cache_write_1h", hour, rates.cache_write_hour)?;
    Ok(cost)
}

fn calculate(
    provider: Provider,
    quote: &Quote,
    usage: &Metrics,
    dimensions: &Dimensions,
) -> Checked<Cost> {
    match provider {
        Provider::OpenAI => openai::calculate(quote, usage, dimensions),
        Provider::Anthropic => anthropic::calculate(quote, usage, dimensions),
        Provider::XAI => xai::calculate(quote, usage, dimensions),
        #[cfg(feature = "workbuddy-lab")]
        Provider::Workbuddy => normalized_cost(
            usage,
            &rates(quote, dimensions, usage.input.ok_or("input_usage_missing")?)?,
            true,
        ),
    }
}

fn result(
    provider: Provider,
    quote: &Quote,
    usage: &Metrics,
    dimensions: &Dimensions,
    error: Option<&'static str>,
) -> Value {
    let mut record = json!({"version":1,"calculator":format!("{}_v1",provider.model_namespace()),"provider":provider,
        "state":"pending","basis":"official_list_price","currency":"USD",
        "official_price_revision":quote.official_price_revision,"official_version":quote.official_version,
        "multiplier_millionths":quote.multiplier_millionths,"dimensions":dimensions,"usage":usage});
    let computed = error
        .map_or_else(|| calculate(provider, quote, usage, dimensions), Err)
        .and_then(|cost| {
            let numerator = cost
                .total
                .checked_mul(u128::from(quote.multiplier_millionths))
                .ok_or("cost_overflow")?;
            let rounded = |value: u128, scale: u128| {
                u64::try_from(value.div_ceil(scale)).map_err(|_| "cost_overflow")
            };
            Ok((
                rounded(numerator, 1_000_000_000_000)?,
                rounded(cost.total, 1_000_000)?,
                numerator,
                cost,
            ))
        });
    match computed {
        Ok((charge, official, numerator, cost)) => {
            record["state"] = json!("calculated");
            record["charge_microusd"] = json!(charge);
            record["official_cost_microusd"] = json!(official);
            record["official_cost_picousd"] = json!(cost.total.to_string());
            record["charge_numerator"] = json!(numerator.to_string());
            record["charge_denominator"] = json!("1000000000000");
            record["rounding"] = json!("ceil_once_to_micro_usd");
            record["items"] = json!(cost.lines);
        }
        Err(reason) => record["reason"] = json!(reason),
    }
    record
}

fn unsupported(reason: &'static str) -> ApiError {
    ApiError::new(400, "billing_unsupported", reason)
}
