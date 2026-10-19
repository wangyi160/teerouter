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
    assert!(!next.serving());
    next.initialize(saved).unwrap();
    assert!(next.lock().unwrap().consumption.is_empty());
    assert_eq!(next.lock().unwrap().balances["user"], -20);
    assert!(next.serving());
    assert_eq!(
        admit(&next.lock().unwrap(), "user").unwrap_err().status,
        402
    );
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

#[test]
fn tier_boundaries_and_invalid_policies() {
    use crate::config::BalanceConcurrency;
    let policy = BalanceConcurrency::default();
    policy.validate().unwrap();
    for (remaining, cap) in [
        (1, 1),
        (1_000_000, 1),
        (1_000_001, 2),
        (5_000_000, 2),
        (5_000_001, 4),
        (10_000_000, 4),
        (10_000_001, 16),
    ] {
        assert_eq!(policy.limit(remaining, 16), cap);
        assert_eq!(policy.limit(remaining, 1), 1);
    }
    for bad in [
        json!([]),
        json!([{"up_to_microusd":1,"max_concurrent":2}]),
        json!([{"up_to_microusd":0,"max_concurrent":1}]),
        json!([{"up_to_microusd":1,"max_concurrent":1},{"up_to_microusd":1,"max_concurrent":2}]),
        json!([{"up_to_microusd":2,"max_concurrent":1},{"up_to_microusd":1,"max_concurrent":2}]),
        json!([{"up_to_microusd":1,"max_concurrent":1},{"up_to_microusd":2,"max_concurrent":4},{"up_to_microusd":3,"max_concurrent":2}]),
        json!([{"up_to_microusd":1,"max_concurrent":1},{"up_to_microusd":2,"max_concurrent":100001}]),
        json!([{"up_to_microusd":u64::MAX,"max_concurrent":1}]),
        json!(
            (1..=6)
                .map(|i| json!({"up_to_microusd":i,"max_concurrent":i}))
                .collect::<Vec<_>>()
        ),
    ] {
        let policy: BalanceConcurrency = serde_json::from_value(json!({"tiers":bad})).unwrap();
        assert!(policy.validate().is_err());
    }
}

struct Admission {
    vault: Arc<Vault>,
    ledger: Arc<Ledger>,
    global: Arc<crate::resources::Gate>,
    keys: Vec<Arc<crate::accounting::Account>>,
}
impl Admission {
    fn new(managed: bool, balance: i64) -> Self {
        let vault = Vault::memory().unwrap();
        let config: crate::config::Config = serde_json::from_value(json!({
            "server":{"allow_empty_routes":true,"admin_key_ref":"admin"},
            "users":[{"id":"user","max_concurrent":16}],
            "keys":[{"name":"one","user_id":"user","key_ref":"first"},
                    {"name":"two","user_id":"user","key_ref":"second"}],
            "models":[]
        }))
        .unwrap();
        config.validate().unwrap();
        vault.lock().unwrap().bundle = Some(crate::vault::Provision {
            config: config.clone(),
            secrets: BTreeMap::from([
                ("first".into(), "secret".into()),
                ("second".into(), "secret".into()),
            ]),
            accounts: BTreeMap::new(),
        });
        if managed {
            vault.require_manager();
            let initial = vault.lock().unwrap().clone();
            vault.initialize(initial).unwrap();
            sync(&vault, balance);
        }
        let keys = config
            .keys
            .into_iter()
            .map(|key| {
                crate::accounting::Account::inherit(key, "secret".into(), vault.clone(), None)
            })
            .collect();
        Self {
            ledger: Arc::new(Ledger::new(vault.clone())),
            vault,
            global: Default::default(),
            keys,
        }
    }
    fn request(&self, key: usize) -> Result<crate::accounting::Ticket, crate::error::ApiError> {
        self.keys[key].admit(
            self.ledger.clone(),
            self.global.clone(),
            64,
            "test".into(),
            "model".into(),
        )
    }
}

#[test]
fn spending_lowers_shared_capacity_immediately_and_topup_restores_it() {
    let a = Admission::new(true, 11_000_000);
    let mut active: Vec<_> = (0..6).map(|i| a.request(i % 2).unwrap()).collect();
    record(&a.ledger, "one", "user", 1_000_000); // Exactly $10: cap 4.
    assert_eq!(
        a.request(1).err().unwrap().code,
        "balance_concurrency_limit"
    );
    assert_eq!(a.vault.user_gate("user").used(), 6); // Existing work is retained.
    for _ in 0..3 {
        active.pop();
    }
    active.push(a.request(1).unwrap());
    assert_eq!(a.request(0).err().unwrap().status, 429);
    record(&a.ledger, "two", "user", 9_000_000); // $1 remaining: cap 1.
    active.clear();
    let one = a.request(0).unwrap();
    assert!(!a.keys[1].has_capacity());
    assert_eq!(
        a.request(1).err().unwrap().code,
        "balance_concurrency_limit"
    );
    drop(one);
    record(&a.ledger, "one", "user", 1_000_000);
    assert_eq!(a.request(0).err().unwrap().code, "insufficient_balance");
    assert_eq!(a.global.used(), 0);
    sync(&a.vault, 20_000_000); // ACK clears only persisted spending; recharge restores cap.
    let held: Vec<_> = (0..16).map(|i| a.request(i % 2).unwrap()).collect();
    assert_eq!(a.request(0).err().unwrap().code, "user_concurrency_limit");
    drop(held);
    assert_eq!(a.global.used(), 0);
    assert_eq!(a.vault.user_gate("user").used(), 0);
}

#[test]
fn simultaneous_keys_cannot_bypass_low_balance_and_child_vaults_are_unaffected() {
    let a = Arc::new(Admission::new(true, 1_000_000));
    let barrier = Arc::new(std::sync::Barrier::new(16));
    let threads: Vec<_> = (0..16)
        .map(|i| {
            let a = a.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                a.request(i % 2)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == "balance_concurrency_limit")
    );
    drop(results);
    assert_eq!(a.global.used(), 0);
    let child = Admission::new(false, 0);
    let held: Vec<_> = (0..16).map(|i| child.request(i % 2).unwrap()).collect();
    assert_eq!(
        child.request(0).err().unwrap().code,
        "user_concurrency_limit"
    );
    drop(held);
}

#[test]
fn live_policy_and_key_recreation_keep_inflight_count_and_lower_key_limits() {
    let a = Admission::new(true, 2_000_000);
    let first = a.request(0).unwrap();
    {
        let mut data = a.vault.lock().unwrap();
        let policy = &mut data.bundle.as_mut().unwrap().config.balance_concurrency;
        policy.tiers[0].up_to_microusd = 3_000_000;
        policy.validate().unwrap();
    }
    let recreated = crate::accounting::Account::inherit(
        a.keys[1].config.clone(),
        "secret".into(),
        a.vault.clone(),
        None,
    );
    assert_eq!(
        recreated
            .admit(
                a.ledger.clone(),
                a.global.clone(),
                64,
                "test".into(),
                "model".into()
            )
            .err()
            .unwrap()
            .code,
        "balance_concurrency_limit"
    );
    drop(first);
    sync(&a.vault, 20_000_000);
    let mut key = a.keys[0].config.clone();
    key.max_concurrent = 1;
    let key = crate::accounting::Account::inherit(
        key,
        "secret".into(),
        a.vault.clone(),
        Some(&a.keys[0]),
    );
    let first = key
        .admit(
            a.ledger.clone(),
            a.global.clone(),
            64,
            "test".into(),
            "model".into(),
        )
        .unwrap();
    assert_eq!(
        key.admit(
            a.ledger.clone(),
            a.global.clone(),
            64,
            "test".into(),
            "model".into()
        )
        .err()
        .unwrap()
        .status,
        429
    );
    assert_eq!(a.vault.user_gate("user").used(), 1);
    drop(first);
}
