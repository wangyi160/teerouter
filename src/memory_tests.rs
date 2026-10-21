use super::*;
fn bundle() -> Provision {
    serde_json::from_value(json!({"config":{"server":{"admin_key_ref":"admin","allow_empty_routes":true},"users":[{"id":"test-user"}],"keys":[{"name":"client","user_id":"test-user","key_ref":"client"}],"models":[]},
        "secrets":{"admin":"admin-secret","client":"client-secret"},"accounts":{"one":{"provider":"openai","credential":{"access_token":"old"},"proxy":"http://127.0.0.1:7897"}}})).unwrap()
}
#[tokio::test]
async fn account_status_reports_jwt_expiry_and_external_renewal_without_maintenance() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let v = Vault::memory().unwrap();
    v.provision(&bundle()).unwrap();
    for (name, expiry, _automatic) in [("expired", 1u64, false), ("valid", 4102444800, false)] {
        let token = format!(
            "e30.{}.fixture",
            URL_SAFE_NO_PAD.encode(json!({"exp":expiry}).to_string())
        );
        let record = json!({"access_token":token});
        v.put_account(name, Provider::XAI, &record).unwrap();
    }
    let engine = crate::router::Engine::from_vault(v).unwrap();
    let status = engine.account_status().unwrap();
    let account = |name| {
        status["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap()
    };
    assert_eq!(account("expired")["health"]["state"], "expired");
    assert_eq!(account("expired")["expires_at"], 1.0);
    assert_eq!(account("expired")["refresh_mode"], "external");
    assert_eq!(account("valid")["health"]["state"], "active");
    assert_eq!(account("valid")["expires_at"], 4102444800.0);
    assert_eq!(account("valid")["refresh_mode"], "external");
    assert!(!status.to_string().contains("fixture"));
}
#[test]
fn checkpoint_tracks_access_updates_and_keeps_concurrent_changes_after_ack() {
    let v = Vault::memory().unwrap();
    v.provision(&bundle()).unwrap();
    v.require_manager();
    let initial = v.lock().unwrap().clone();
    v.initialize(initial).unwrap();
    v.put_account("one", Provider::OpenAI, &json!({"access_token":"new"}))
        .unwrap();
    let ledger = crate::ledger::Ledger::new(v.clone());
    ledger.record(
        "client",
        &crate::ledger::Usage {
            requests: 1,
            ..Default::default()
        },
        &json!({"request_id":"first"}),
        None,
    );
    let saved = v.checkpoint().unwrap();
    assert_eq!(
        saved["state"]["bundle"]["accounts"]["one"]["credential"]["access_token"],
        "new"
    );
    ledger.record(
        "client",
        &crate::ledger::Usage {
            requests: 1,
            ..Default::default()
        },
        &json!({"request_id":"second"}),
        None,
    );
    assert!(
        v.acknowledge(
            "wrong",
            saved["state"]["generation"].as_u64().unwrap(),
            1,
            BTreeMap::new()
        )
        .is_err()
    );
    assert!(v.serving());
    v.acknowledge(
        &v.boot_id,
        saved["state"]["generation"].as_u64().unwrap(),
        1,
        BTreeMap::new(),
    )
    .unwrap();
    assert!(v.serving());
    let remaining = v.checkpoint().unwrap();
    assert_eq!(remaining["state"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        remaining["state"]["events"][0]["event"]["request_id"],
        "second"
    );
    let next = Vault::memory().unwrap();
    next.require_manager();
    assert!(!next.serving());
    let mut recovered = saved["state"].clone();
    // Management archives receipts before restoring configuration/counters.
    recovered["events"] = json!([]);
    next.initialize(serde_json::from_value(recovered).unwrap())
        .unwrap();
    assert_ne!(next.boot_id, v.boot_id);
    assert!(next.serving());
    assert_eq!(
        next.account("one", Provider::OpenAI).unwrap().0["access_token"],
        "new"
    );
    assert!(next.acknowledge(&v.boot_id, 0, 0, BTreeMap::new()).is_err());
}
#[test]
fn access_cas_atomic_updates_and_identity_affinity_are_preserved() {
    let v = Vault::memory().unwrap();
    v.provision(&bundle()).unwrap();
    v.bind_session(
        "client",
        "response",
        "response-1",
        "oauth:openai:one",
        60,
        100,
    )
    .unwrap();
    let snapshot = v.snapshot().unwrap();
    let update = operations::Update {
        accounts: BTreeMap::from([(
            "one".into(),
            json!({"credential":{"access_token":"replacement"}}),
        )]),
        ..Default::default()
    };
    let candidate = snapshot.updated(&update).unwrap();
    assert!(v.commit_update(&snapshot, &candidate, &update).unwrap());
    assert!(!v.commit_update(&snapshot, &candidate, &update).unwrap());
    assert_eq!(
        v.binding("client", "response", "response-1")
            .unwrap()
            .as_deref(),
        Some("!retired")
    );
    let before = v.checkpoint().unwrap();
    let mut bad = bundle();
    bad.accounts.get_mut("one").unwrap().provider = Provider::Anthropic;
    assert!(v.provision(&bad).is_err());
    assert_eq!(v.checkpoint().unwrap(), before);
}
#[test]
fn management_cannot_restore_official_catalog_or_session_bindings() {
    let v = Vault::memory().unwrap();
    v.provision(&bundle()).unwrap();
    let mut snapshot = v.checkpoint().unwrap()["state"].clone();
    for name in ["catalog", "sessions", "health"] {
        snapshot[name] = json!({"forged":true});
        assert!(serde_json::from_value::<Data>(snapshot.clone()).is_err());
        snapshot.as_object_mut().unwrap().remove(name);
    }
}

#[test]
fn manager_disconnect_does_not_expire_initialized_router() {
    let v = Vault::memory().unwrap();
    v.provision(&bundle()).unwrap();
    v.require_manager();
    assert!(!v.serving());
    let data = v.lock().unwrap().clone();
    v.initialize(data).unwrap();
    assert!(v.serving()); // No ACK is needed to activate restored state.
    assert!(!v.manager_connected());
    let checkpoint = v.checkpoint().unwrap();
    v.acknowledge(
        &v.boot_id,
        checkpoint["state"]["generation"].as_u64().unwrap(),
        0,
        BTreeMap::new(),
    )
    .unwrap();
    assert!(v.serving());
    assert!(v.manager_connected());
    for seconds in [631, 3600, 86400 * 7] {
        v.sync.lock().unwrap().acknowledged =
            Some(Instant::now() - std::time::Duration::from_secs(seconds));
        assert!(!v.manager_connected());
        assert!(v.serving());
    }
    v.lock().unwrap().balances.insert("user".into(), 100);
    let ledger = crate::ledger::Ledger::new(v.clone());
    ledger.record(
        "client",
        &crate::ledger::Usage::default(),
        &json!({"user_id":"user","billing":{"state":"calculated","charge_microusd":40}}),
        None,
    );
    {
        let data = v.lock().unwrap();
        assert_eq!(data.balances["user"], 100);
        assert_eq!(data.consumption["user"], 40);
        assert!(crate::balances::admit(&data, "user").is_ok());
    }
    let saved = v.checkpoint().unwrap();
    v.acknowledge(
        &v.boot_id,
        saved["state"]["generation"].as_u64().unwrap(),
        saved["state"]["sequence"].as_u64().unwrap(),
        BTreeMap::from([("user".into(), 60)]),
    )
    .unwrap();
    assert_eq!(v.lock().unwrap().balances["user"], 60);
    assert!(v.lock().unwrap().consumption.is_empty());
}
