use super::*;

pub(super) fn calculate(quote: &Quote, usage: &Metrics, dimensions: &Dimensions) -> Checked<Cost> {
    // Claude input_tokens excludes BOTH cache reads and writes. The context
    // threshold includes them, while ordinary input must not be reduced again.
    let input = usage.input.ok_or("input_usage_missing")?;
    let total = input
        .checked_add(usage.cache_read.unwrap_or(0))
        .and_then(|n| n.checked_add(usage.cache_write.unwrap_or(0)))
        .ok_or("usage_overflow")?;
    let mut cost = normalized_cost(usage, &rates(quote, dimensions, total)?, false)?;
    cost.tools(Provider::Anthropic, usage, &quote.billing_rules)?;
    Ok(cost)
}
