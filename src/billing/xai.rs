use super::*;

pub(super) fn calculate(quote: &Quote, usage: &Metrics, dimensions: &Dimensions) -> Checked<Cost> {
    let input = usage.input.ok_or("input_usage_missing")?;
    let mut normalized = usage.clone();
    let output = usage.output.ok_or("output_usage_missing")?;
    let reasoning = usage.reasoning.unwrap_or(0);
    if reasoning > 0 {
        // Legacy xAI Chat separates completion and reasoning tokens; modern
        // Responses includes reasoning in output_tokens. The official total
        // disambiguates these formats, rather than assuming OpenAI semantics.
        let total = usage.total.ok_or("reasoning_total_missing")?;
        let plain = input.checked_add(output).ok_or("usage_overflow")?;
        if total == plain.checked_add(reasoning).ok_or("usage_overflow")? {
            normalized.output = Some(output.checked_add(reasoning).ok_or("usage_overflow")?);
        } else if total != plain || reasoning > output {
            return Err("inconsistent_reasoning_usage");
        }
    } else if usage
        .total
        .is_some_and(|total| input.checked_add(output) != Some(total))
    {
        return Err("inconsistent_total_usage");
    }
    let mut cost = normalized_cost(&normalized, &rates(quote, dimensions, input)?, true)?;
    cost.tools(Provider::XAI, usage, &quote.billing_rules)?;
    // cost_in_usd_ticks is preserved as evidence, NOT used as a public list-price
    // quote: it can include private discounts and subscription/account benefits.
    Ok(cost)
}
