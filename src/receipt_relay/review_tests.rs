//! Bounded execution of production operations, not a reimplementation of the protocol.
use super::*;
use std::sync::Arc;

const KEY: [u8; 32] = [61; 32];
fn fresh(dir: &std::path::Path) -> Arc<Vault> {
    let v = Vault::with_journal(dir, &KEY).unwrap();
    v.require_manager();
    v.initialize(Data {
        instance: "ab".repeat(16),
        balances: BTreeMap::from([("a".into(), 1000), ("b".into(), 1000)]),
        ..Default::default()
    })
    .unwrap();
    v.configure_relay(Config {
        url: "https://example.org".into(),
        upload_token: "t".repeat(64),
        interval_seconds: 300,
        user_ids: BTreeMap::from([("a".into(), "12".repeat(32)), ("b".into(), "34".repeat(32))]),
    })
    .unwrap();
    v
}

fn record(v: &Arc<Vault>, user: &str, cost: u64) {
    crate::ledger::Ledger::new(v.clone()).record(user, &crate::ledger::Usage { requests: 1, ..Default::default() },
        &json!({"user_id":user,"key":user,"provider":"xai","model":"grok-4.7","status":200,"attempts":1,
            "usage":{"input_tokens":1,"output_tokens":1},
            "billing":{"state":"calculated","charge_microusd":cost}}), None);
}

fn recovery(v: &Vault) -> Data {
    let mut saved = v.lock().unwrap().clone();
    let relay = saved.receipt_relay.as_mut().unwrap();
    saved.sequence = relay
        .settled
        .as_ref()
        .map_or(relay.start_sequence, |h| h.through_seq);
    saved.events.clear();
    saved.consumption.clear();
    relay.uploaded = relay.settled.clone();
    relay.retained.clear();
    saved
}

#[test]
fn bounded_journal_relay_traces_conserve_two_user_balances() {
    // Alphabet: finish(a,7), finish(b,11), upload, settle, restart.
    // Every length-four trace is replayed against a fresh real SQLite journal.
    let mut checked = 0;
    for encoded in 0..5usize.pow(4) {
        let dir = tempfile::tempdir().unwrap();
        let mut v = fresh(dir.path());
        let mut events = Vec::<(&str, u64)>::new();
        let mut trace = encoded;
        for step in 0..4 {
            let action = trace % 5;
            trace /= 5;
            match action {
                0 | 1 => {
                    let (user, cost) = if action == 0 { ("a", 7) } else { ("b", 11) };
                    record(&v, user, cost);
                    events.push((user, cost));
                }
                2 => {
                    if let Some((_, body)) = v.prepare_public(true).unwrap() {
                        // The environment supplies a truthful durable storage ACK.
                        v.uploaded_public(body["batch_hash"].as_str().unwrap())
                            .unwrap();
                    }
                }
                3 => {
                    let state = v.lock().unwrap().receipt_relay.clone().unwrap();
                    let through = state.uploaded.as_ref().map_or(0, |h| h.through_seq);
                    let mut balances = BTreeMap::from([("a".into(), 1000), ("b".into(), 1000)]);
                    for (user, cost) in events.iter().take(through as usize) {
                        *balances.get_mut(*user).unwrap() -= *cost as i64;
                    }
                    let ack = Settlement {
                        boot_id: v.boot_id.clone(),
                        revision: state.balance_revision + 1,
                        through: state.uploaded,
                        balances: Some(balances),
                        balances_patch: None,
                    };
                    let retry = serde_json::to_value(&ack).unwrap();
                    v.relay_settle(ack).unwrap();
                    v.relay_settle(serde_json::from_value(retry).unwrap())
                        .unwrap();
                }
                _ => {
                    let saved = recovery(&v);
                    drop(v);
                    v = Vault::with_journal(dir.path(), &KEY).unwrap();
                    v.require_manager();
                    v.initialize(saved).unwrap();
                }
            }
            let data = v.lock().unwrap();
            assert_eq!(
                data.sequence,
                events.len() as u64,
                "trace={encoded} step={step}"
            );
            for user in ["a", "b"] {
                let total: u64 = events.iter().filter(|e| e.0 == user).map(|e| e.1).sum();
                assert_eq!(
                    crate::balances::available(&data, user).unwrap(),
                    1000 - total,
                    "trace={encoded} step={step} user={user}"
                );
            }
            checked += 1;
        }
    }
    println!("CODE_EXPLORATION traces=625 transitions={checked} users=2 max_steps=4");
    assert_eq!(checked, 2500);
}

#[test]
fn checkpoint_pulls_never_block_relay_activation() {
    let v = Vault::memory().unwrap();
    v.require_manager();
    v.initialize(Data::default()).unwrap();
    let first = v.checkpoint_export(None, None).unwrap();
    assert!(first.get("receipt_batch").is_none());
    v.checkpoint_export(Some(&v.boot_id), first["checkpoint_id"].as_str()).unwrap();
    let result = v.configure_relay(Config {
        url: "https://example.org".into(),
        upload_token: "t".repeat(64),
        interval_seconds: 300,
        user_ids: BTreeMap::new(),
    });
    result.unwrap();
    assert!(v.lock().unwrap().receipt_relay.is_some());
}

#[test]
fn settlement_rejections_are_atomic_and_keep_new_tail() {
    let dir = tempfile::tempdir().unwrap();
    let v = fresh(dir.path());
    record(&v, "a", 7);
    let (_, body) = v.prepare_public(true).unwrap().unwrap();
    let pending = v
        .relay_publishing
        .lock()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .head
        .clone();
    let ack = json!({"boot_id":v.boot_id,"revision":1,"through":pending,
        "balances":{"a":993,"b":1000},"balances_patch":null});
    let before = serde_json::to_value(&*v.lock().unwrap()).unwrap();
    assert!(
        v.relay_settle(serde_json::from_value(ack.clone()).unwrap())
            .is_err()
    );
    assert_eq!(serde_json::to_value(&*v.lock().unwrap()).unwrap(), before);
    v.uploaded_public(body["batch_hash"].as_str().unwrap())
        .unwrap();
    record(&v, "b", 11);
    v.relay_settle(serde_json::from_value(ack.clone()).unwrap())
        .unwrap();
    let before = serde_json::to_value(&*v.lock().unwrap()).unwrap();
    for (key, value) in [
        ("boot_id", json!("stale-boot")),
        ("balances", json!({"a":999,"b":1000})),
    ] {
        let mut wrong = ack.clone();
        wrong[key] = value;
        assert!(
            v.relay_settle(serde_json::from_value(wrong).unwrap())
                .is_err()
        );
        assert_eq!(serde_json::to_value(&*v.lock().unwrap()).unwrap(), before);
    }
    assert_eq!(v.lock().unwrap().consumption["b"], 11);
}

#[test]
fn relay_balance_patch_preserves_other_users_and_rejects_conflicting_retry() {
    let dir = tempfile::tempdir().unwrap();
    let v = fresh(dir.path());
    let ack = json!({"boot_id":v.boot_id,"revision":1,"through":null,
        "balances":null,"balances_patch":{"a":70}});
    for _ in 0..2 { v.relay_settle(serde_json::from_value(ack.clone()).unwrap()).unwrap(); }
    assert_eq!(v.lock().unwrap().balances, BTreeMap::from([("a".into(),70),("b".into(),1000)]));
    let mut bad = ack.clone(); bad["balances_patch"]["a"] = 60.into();
    assert!(v.relay_settle(serde_json::from_value(bad).unwrap()).is_err());
    let mut remove = ack; remove["revision"] = 2.into(); remove["balances_patch"] = json!({"b":null});
    v.relay_settle(serde_json::from_value(remove).unwrap()).unwrap();
    assert_eq!(v.lock().unwrap().balances,BTreeMap::from([("a".into(),70)]));
}
