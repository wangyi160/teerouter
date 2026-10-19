//! Last synchronized balance minus unsynchronized consumption. No remote calls.
use crate::{error::ApiError, vault::Data};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};

fn charge(event: &Value) -> Option<(&str, u64)> {
    let result = &event["billing"];
    if result["state"] != "calculated" {
        return None;
    }
    Some((
        event["user_id"].as_str()?,
        result["charge_microusd"].as_u64()?,
    ))
}

pub(crate) fn add(spending: &mut BTreeMap<String, u128>, event: &Value) {
    if let Some((user, cost)) = charge(event) {
        let total = spending.entry(user.to_owned()).or_default();
        *total = total.saturating_add(u128::from(cost));
    }
}

pub(crate) fn consumption(events: &VecDeque<Value>) -> BTreeMap<String, u128> {
    let mut spending = BTreeMap::new();
    for row in events {
        add(&mut spending, &row["event"]);
    }
    spending
}

pub(crate) fn available(data: &Data, user: &str) -> Result<u64, ApiError> {
    let balance = data.balances.get(user).ok_or_else(|| {
        ApiError::new(
            503,
            "balance_unavailable",
            "User balance has not been synchronized",
        )
    })?;
    let consumed = data.consumption.get(user).copied().unwrap_or(0);
    if *balance <= 0 || consumed >= *balance as u128 {
        return Err(ApiError::new(
            402,
            "insufficient_balance",
            "User balance is insufficient",
        ));
    }
    Ok(*balance as u64 - consumed as u64)
}

pub(crate) fn concurrency_limit(
    data: &Data,
    user: &crate::config::User,
    policy: &crate::config::BalanceConcurrency,
) -> Result<usize, ApiError> {
    Ok(policy.limit(available(data, &user.id)?, user.max_concurrent))
}

#[cfg(test)]
pub(crate) fn admit(data: &Data, user: &str) -> Result<(), ApiError> {
    available(data, user).map(|_| ())
}

#[cfg(test)]
mod tests;
