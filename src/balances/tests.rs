use super::*;
use crate::{
    ledger::{Ledger, Usage},
    vault::Vault,
};
use serde_json::json;
use std::sync::Arc;

fn managed() -> Arc<Vault> {
    let vault = Vault::memory().unwrap();
    vault.require_manager();
    let initial = vault.lock().unwrap().clone();
    vault.initialize(initial).unwrap();
    sync(&vault, 100);
    vault
}
fn ack(vault: &Vault, checkpoint: &Value, balance: i64) -> Result<(), crate::vault::VaultError> {
    vault.acknowledge(
        &vault.boot_id,
        checkpoint["state"]["generation"].as_u64().unwrap(),
        checkpoint["state"]["sequence"].as_u64().unwrap(),
        BTreeMap::from([("user".into(), balance)]),
    )
}
fn sync(vault: &Vault, balance: i64) {
    ack(vault, &vault.checkpoint().unwrap(), balance).unwrap();
}
fn record(ledger: &Ledger, key: &str, user: &str, cost: u64) {
    ledger.record(
        key,
        &Usage::default(),
        &json!({"user_id":user,"billing":{"state":"calculated","charge_microusd":cost}}),
        None,
    );
}

#[test]
fn requests_only_accumulate_spend_shared_by_all_user_keys() {
    let v = managed();
    let ledger = Ledger::new(v.clone());
    record(&ledger, "first-key", "user", 40);
    record(&ledger, "other-key", "user", 59);
    assert!(admit(&v.lock().unwrap(), "user").is_ok());
    record(&ledger, "third-key", "other-user", 200);
    record(&ledger, "first-key", "user", 1);
    let data = v.lock().unwrap();
    assert_eq!(data.balances["user"], 100);
    assert_eq!(data.consumption["user"], 100);
    let error = admit(&data, "user").unwrap_err();
    assert_eq!((error.status, error.code), (402, "insufficient_balance"));
    assert_eq!(
        admit(&data, "missing").unwrap_err().code,
        "balance_unavailable"
    );
}

#[test]
fn ack_clears_only_persisted_spending_and_retries_do_not_change_balance() {
    let v = managed();
    let ledger = Ledger::new(v.clone());
    record(&ledger, "key", "user", 30);
    let snapshot = v.checkpoint().unwrap();
    // This request finishes while management persists the first receipt.
    record(&ledger, "key", "user", 20);
    ack(&v, &snapshot, 70).unwrap();
    ack(&v, &snapshot, 70).unwrap(); // A lost response can be retried.
    assert!(ack(&v, &snapshot, 100).is_err());
    {
        let data = v.lock().unwrap();
        assert_eq!(data.balances["user"], 70);
        assert_eq!(data.consumption["user"], 20);
        assert_eq!(data.events.len(), 1);
        assert_eq!(data.events[0]["seq"], 2);
    }
    sync(&v, 50);
    assert!(ack(&v, &snapshot, 70).is_err());
    assert!(v.lock().unwrap().consumption.is_empty());
    // A later deposit is valid even when no new receipt was generated.
    sync(&v, 150);
    assert_eq!(v.lock().unwrap().balances["user"], 150);
}

#[test]
fn negative_balance_and_restart_use_the_last_durable_balance() {
    let v = managed();
    let ledger = Ledger::new(v.clone());
    record(&ledger, "key", "user", 120);
    sync(&v, -20);
    assert_eq!(admit(&v.lock().unwrap(), "user").unwrap_err().status, 402);
    let mut saved = v.lock().unwrap().clone();
    saved.consumption.insert("user".into(), 999); // Derived, never trusted on restore.
    let next = Vault::memory().unwrap();
    next.require_manager();
    next.initialize(saved).unwrap();
    assert!(next.lock().unwrap().consumption.is_empty());
    assert_eq!(next.lock().unwrap().balances["user"], -20);
    assert!(!next.serving());
    sync(&next, 80);
    assert!(next.serving());
    assert!(admit(&next.lock().unwrap(), "user").is_ok());
}

#[test]
fn pending_usage_does_not_invent_a_charge_and_large_sums_do_not_wrap() {
    let merchant = Vault::memory().unwrap();
    record(&Ledger::new(merchant.clone()), "key", "user", 100);
    assert!(merchant.lock().unwrap().consumption.is_empty());
    let v = managed();
    let ledger = Ledger::new(v.clone());
    ledger.record(
        "key",
        &Usage::default(),
        &json!({"user_id":"user","billing":{"state":"pending","charge_microusd":100}}),
        None,
    );
    assert!(v.lock().unwrap().consumption.is_empty());
    record(&ledger, "key", "user", u64::MAX);
    record(&ledger, "key", "user", u64::MAX);
    assert_eq!(
        v.lock().unwrap().consumption["user"],
        u128::from(u64::MAX) * 2
    );
    let checkpoint = v.checkpoint().unwrap();
    let restored: Data = serde_json::from_value(checkpoint["state"].clone()).unwrap();
    assert_eq!(restored.consumption["user"], u128::from(u64::MAX) * 2);
}
