use super::*;

pub(super) fn calculate(quote: &Quote, usage: &Metrics, dimensions: &Dimensions) -> Checked<Cost> {
    // Responses/Chat usage includes cached input and reasoning output. Never add
    // reasoning_tokens again: it is a breakdown of the billed output total.
    let input = usage.input.ok_or("input_usage_missing")?;
    let output = usage.output.ok_or("output_usage_missing")?;
    if usage.reasoning.unwrap_or(0) > output {
        return Err("inconsistent_reasoning_usage");
    }
    if usage
        .total
        .is_some_and(|total| input.checked_add(output) != Some(total))
    {
        return Err("inconsistent_total_usage");
    }
    let mut cost = normalized_cost(usage, &rates(quote, dimensions, input)?, true)?;
    cost.tools(Provider::OpenAI, usage, &quote.billing_rules)?;
    Ok(cost)
}
