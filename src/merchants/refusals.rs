//! Per-user, per-merchant refusal totals. No prompt or refusal text is retained.
use super::*;

pub(crate) type Counts = BTreeMap<String, BTreeMap<String, u64>>;

pub(super) fn rejection() -> ApiError {
    ApiError::new(
        403,
        "merchant_refusal_limit_exceeded",
        "User disabled for this merchant after exceeding the refusal limit",
    )
}

pub(super) fn status(
    runtime: &Runtime,
    merchant: &str,
    customer: &Customer,
    settings: &Settings,
) -> Result<Value, ApiError> {
    let engine = runtime.engine();
    let data = engine.vault.lock().map_err(storage)?;
    let total = data
        .merchant_refusal_counts
        .get(merchant)
        .and_then(|users| users.get(&customer.user_id))
        .copied()
        .unwrap_or(0);
    let count = total.saturating_sub(customer.refusal_baseline);
    let limit = customer
        .refusal_limit
        .unwrap_or(settings.customer_defaults.refusal_limit);
    Ok(json!({"total":total,"count":count,"limit":limit,"blocked":limit > 0 && count > limit}))
}

pub(super) fn blocked(
    runtime: &Runtime,
    merchant: &str,
    user: &str,
    settings: &Settings,
) -> Result<bool, ApiError> {
    Ok(status(runtime, merchant, &settings.customer(user), settings)?["blocked"] == true)
}

pub(super) fn check(
    runtime: &Runtime,
    merchant: &str,
    user: &str,
    settings: &Settings,
) -> Result<(), ApiError> {
    if blocked(runtime, merchant, user, settings)? {
        Err(rejection())
    } else {
        Ok(())
    }
}
