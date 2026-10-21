use super::*;
use crate::vault::Vault;
use axum::{Json, Router, extract::State, routing::put};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn config(url: &str) -> Config {
    Config {
        url: url.into(),
        upload_token: "t".repeat(64),
        interval_seconds: 300,
        user_ids: BTreeMap::from([("u".into(), "12".repeat(32))]),
    }
}
fn vault() -> Arc<Vault> {
    let v = Vault::memory().unwrap();
    v.require_manager();
    let mut data = Data::default();
    data.instance = "ab".repeat(16);
    data.balances.insert("u".into(), 1000);
    v.initialize(data).unwrap();
    v.configure_relay(config("https://example.org")).unwrap();
    v
}
fn append(v: &Vault, amount: u64) {
    let mut d = v.lock().unwrap();
    d.sequence += 1;
    let seq = d.sequence;
    let event = json!({"user_id":"u","key":"private-key","request_id":"private-request",
        "model":"grok-4.7","pool":"grok","provider":"xai","merchant_id":"merchant",
        "status":200,"attempts":1,"created_at":1,"started_at_ms":1000,"duration_ms":5,
        "usage":{"requests":1,"errors":0,"input_tokens":1,"output_tokens":2,"unknown_usage_requests":0},
        "cache_usage":{"read":0,"write":0,"write_hour":0},
        "refusal_type":"refusal","refusal":{"text":"private refusal"},
        "billing":{"version":1,"state":"calculated","charge_microusd":amount}});
    crate::balances::add(&mut d.consumption, &event);
    d.events.push_back(json!({"seq":seq,"event":event}));
}
fn upload(v: &Vault) -> Head {
    let (_, body) = v.prepare_public(true).unwrap().unwrap();
    v.uploaded_public(body["batch_hash"].as_str().unwrap())
        .unwrap();
    v.lock()
        .unwrap()
        .receipt_relay
        .as_ref()
        .unwrap()
        .uploaded
        .clone()
        .unwrap()
}
fn settle(v: &Vault, revision: u64, head: Option<Head>, balance: i64) -> Result<()> {
    v.relay_settle(Settlement {
        boot_id: v.boot_id.clone(),
        revision,
        through: head,
        balances: Some(BTreeMap::from([("u".into(), balance)])),
        balances_patch: None,
    })
}
#[test]
fn storage_ack_preserves_spend_and_settlement_preserves_newer_requests() {
    let v = vault();
    append(&v, 10);
    let first = upload(&v);
    assert!(v.lock().unwrap().events.is_empty());
    assert_eq!(
        crate::balances::available(&v.lock().unwrap(), "u").unwrap(),
        990
    );
    append(&v, 20);
    let second = upload(&v);
    append(&v, 30);
    settle(&v, 1, Some(first.clone()), 990).unwrap();
    assert_eq!(v.lock().unwrap().consumption["u"], 50);
    assert_eq!(
        crate::balances::available(&v.lock().unwrap(), "u").unwrap(),
        940
    );
    settle(&v, 1, Some(first.clone()), 990).unwrap(); // Lost response, no second clearing.
    assert!(settle(&v, 1, Some(first.clone()), 999).is_err());
    assert!(settle(&v, 2, None, 1000).is_err());
    settle(&v, 2, Some(first), 1090).unwrap(); // Deposit at the same settlement boundary.
    assert_eq!(v.lock().unwrap().consumption["u"], 50);
    settle(&v, 3, Some(second), 1070).unwrap();
    assert_eq!(v.lock().unwrap().consumption["u"], 30);
    assert!(
        v.acknowledge_receipts(
            &v.boot_id,
            0,
            0,
            Some(BTreeMap::new()),
            BTreeMap::new(),
            None
        )
        .is_err()
    );
}
#[test]
fn frozen_retries_and_public_records_have_rotating_pseudonyms_no_private_fields() {
    let v = vault();
    append(&v, 10);
    let (_, first) = v.prepare_public(true).unwrap().unwrap();
    append(&v, 20);
    assert_eq!(first, v.prepare_public(true).unwrap().unwrap().1);
    let text = serde_json::to_string(&first).unwrap();
    for private in [
        "private-key",
        "private-request",
        "private refusal",
        "user_id",
        "refusal_id",
    ] {
        assert!(!text.contains(private), "{private}");
    }
    let a: Vec<Value> = serde_json::from_str(first["records_json"].as_str().unwrap()).unwrap();
    v.uploaded_public(first["batch_hash"].as_str().unwrap())
        .unwrap();
    let (_, next) = v.prepare_public(true).unwrap().unwrap();
    let b: Vec<Value> = serde_json::from_str(next["records_json"].as_str().unwrap()).unwrap();
    assert_ne!(a[0]["user_hash"], b[0]["user_hash"]);
    assert_ne!(a[0]["key_hash"], b[0]["key_hash"]);
    assert_eq!(a[0]["refusal_type"], "refusal");
    let manifest: Value = serde_json::from_str(next["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(manifest["previous_batch_hash"], first["batch_hash"]);
    assert_eq!(manifest["sequence_before"], 1);
    assert_eq!(manifest["sequence_through"], 2);
}

#[test]
fn public_wire_fixture_for_independent_consumer() {
    let Some(path) = std::env::var_os("TEEROUTER_PUBLIC_RECEIPT_FIXTURE") else {
        return;
    };
    let v = vault();
    append(&v, 10);
    let (_, body) = v.prepare_public(true).unwrap().unwrap();
    let output = json!({"batch":body,"identity":{"schema":"teerouter.receipt-key.v1",
        "boot_id":v.boot_id,"public_key":v.receipt_signer.public_key(),"evidence_type":"unattested_test"}});
    std::fs::write(path, serde_json::to_vec(&output).unwrap()).unwrap();
}

#[test]
fn billing_v2_wire_fixture_preserves_evidence_without_text() {
    use crate::{
        Provider,
        billing::{Observer, aggregate},
        upstream::Protocol,
    };
    let mut cases = vec![];
    for (provider, protocol) in [
        (Provider::OpenAI, Protocol::Chat),
        (Provider::Anthropic, Protocol::Messages),
        (Provider::XAI, Protocol::Chat),
    ] {
        let o = Observer::new(
            provider,
            &json!({"messages":[{"role":"user","content":"PRIVATE_INPUT hello"}]}),
            crate::billing::tests::quote(),
        )
        .unwrap();
        let text = if protocol == Protocol::Messages {
            json!({"type":"content_block_delta","index":0,"delta":{"text":"PRIVATE_REPLY hello"}})
        } else {
            json!({"choices":[{"index":0,"delta":{"content":"PRIVATE_REPLY hello"}}]})
        };
        o.observe(protocol, &text, true);
        cases.push((
            499,
            aggregate(
                &[json!({"number":1,"status":499,"billing":o.finish(499,1)})],
                1,
            ),
        ));
        let failed = o.fresh();
        failed.headers(200);
        failed.observe(protocol, &text, true);
        failed.observe(protocol, &json!({"error":{"code":"server_error"}}), true);
        cases.push((
            200,
            aggregate(
                &[json!({"number":1,"status":200,"billing":failed.finish(200,1)})],
                1,
            ),
        ));
        o.observe(protocol, &if protocol == Protocol::Messages {
            json!({"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}})
        } else { json!({"usage":{"prompt_tokens":10,"completion_tokens":5}}) }, false);
        cases.push((
            502,
            aggregate(
                &[json!({"number":1,"status":502,"billing":o.finish(502,1)})],
                1,
            ),
        ));
    }
    let v = vault();
    let mut events = vec![];
    for (status, billing) in cases {
        let tokens = crate::billing::tokens(&billing);
        let event = json!({"event":"request","user_id":"u","key":"private-key","request_id":"PRIVATE_REQUEST_ID",
            "model":"model-test","pool":"pool","provider":billing["provider"],"merchant_id":"m_fixture",
            "status":status,"attempts":1,"created_at":1,"started_at_ms":1000,"duration_ms":5,
            "usage":{"requests":1,"errors":1,"input_tokens":tokens.input,"output_tokens":tokens.output,"unknown_usage_requests":u64::from(!tokens.known)},
            "cache_usage":{"read":tokens.cache_read,"write":tokens.cache_write,"write_hour":tokens.cache_write_hour},
            "merchant_price_version":1,"merchant_pricing":{"basis":"official_pool_multiplier","official_price_revision":3,"official_version":2,"multiplier_millionths":800000},
            "billing":billing});
        let mut d = v.lock().unwrap();
        d.sequence += 1;
        let seq = d.sequence;
        crate::balances::add(&mut d.consumption, &event);
        d.events.push_back(json!({"seq":seq,"event":event}));
        events.push(event);
    }
    let (_, batch) = v.prepare_public(true).unwrap().unwrap();
    let records: Vec<Value> =
        serde_json::from_str(batch["records_json"].as_str().unwrap()).unwrap();
    assert_eq!(records.len(), 9);
    assert!(!batch.to_string().contains("PRIVATE_"));
    assert!(
        records
            .iter()
            .all(|r| r["billing"]["attempt_records"][0]["billing"]["usage_source"].is_string())
    );
    if let Some(path) = std::env::var_os("TEEROUTER_BILLING_FIXTURES") {
        std::fs::write(path, serde_json::to_vec(&events).unwrap()).unwrap();
    }
    if let Some(path) = std::env::var_os("TEEROUTER_BILLING_PUBLIC_FIXTURE") {
        std::fs::write(path,serde_json::to_vec(&json!({"batch":batch,"identity":{"schema":"teerouter.receipt-key.v1",
            "boot_id":v.boot_id,"public_key":v.receipt_signer.public_key(),"evidence_type":"unattested_test"}})).unwrap()).unwrap();
    }
}
#[test]
fn configuration_and_recovery_reject_identity_changes_and_unsettled_replay() {
    let v = vault();
    let mut invalid = config("http://example.org");
    assert!(v.configure_relay(invalid.clone()).is_err());
    invalid.url = "https://example.org?token=x".into();
    assert!(v.configure_relay(invalid.clone()).is_err());
    invalid = config("https://example.org");
    invalid.user_ids.insert("u".into(), "34".repeat(32));
    assert!(v.configure_relay(invalid).is_err());
    append(&v, 10);
    let head = upload(&v);
    let mut snapshot = v.lock().unwrap().clone();
    let fresh = Vault::memory().unwrap();
    fresh.require_manager();
    assert!(fresh.initialize(snapshot.clone()).is_err());
    snapshot.receipt_relay.as_mut().unwrap().retained.clear();
    snapshot.receipt_relay.as_mut().unwrap().settled = Some(head);
    snapshot.balances.insert("u".into(), 990);
    fresh.initialize(snapshot).unwrap();
    assert!(fresh.lock().unwrap().consumption.is_empty());
}
#[tokio::test]
async fn network_retry_keeps_exact_batch_and_validates_ack_before_release() {
    #[derive(Clone)]
    struct Mock {
        reject: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }
    async fn handler(State(m): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
        m.seen.lock().unwrap().push(body.clone());
        Json(
            json!({"durable":true,"duplicate":false,"detail_state":"pending","settlement_acknowledged":false,
            "batch_hash":if m.reject.load(Ordering::SeqCst) {"wrong".into()} else {body["batch_hash"].clone()}}),
        )
    }
    let m = Mock {
        reject: Arc::new(AtomicBool::new(true)),
        seen: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/v1/streams/{s}/batches/{n}", put(handler))
                .with_state(m.clone()),
        )
        .into_future(),
    );
    let v = Vault::memory().unwrap();
    v.require_manager();
    let mut d = Data::default();
    d.instance = "ab".repeat(16);
    v.initialize(d).unwrap();
    v.configure_relay(config(&url)).unwrap();
    append(&v, 10);
    assert!(v.publish_public(true).await.is_err());
    assert_eq!(v.lock().unwrap().events.len(), 1);
    append(&v, 20);
    m.reject.store(false, Ordering::SeqCst);
    v.publish_public(true).await.unwrap();
    assert_eq!(v.lock().unwrap().events.len(), 1);
    assert_eq!(v.lock().unwrap().consumption["u"], 30);
    let seen = m.seen.lock().unwrap();
    assert_eq!(seen[0], seen[1]);
    server.abort();
}
