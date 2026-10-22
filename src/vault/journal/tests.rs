use super::*;
use axum::{Json, Router, extract::State, routing::put};
use std::sync::atomic::{AtomicUsize, Ordering};

const KEY: [u8; 32] = [27; 32];
fn baseline() -> Data {
    Data {
        instance: "ab".repeat(16),
        balances: BTreeMap::from([("u".into(), 1000)]),
        ..Default::default()
    }
}
fn create(dir: &Path, url: &str) -> Arc<Vault> {
    let v = Vault::with_journal(dir, &KEY).unwrap();
    v.require_manager();
    v.initialize(baseline()).unwrap();
    v.configure_relay(crate::receipt_relay::Config {
        url: url.into(),
        upload_token: "t".repeat(64),
        interval_seconds: 300,
        user_ids: BTreeMap::from([("u".into(), "12".repeat(32))]),
    })
    .unwrap();
    v
}
fn record(v: &Arc<Vault>, amount: u64) {
    let usage = crate::ledger::Usage {
        requests: 1,
        input_tokens: 1,
        output_tokens: 2,
        ..Default::default()
    };
    crate::ledger::Ledger::new(v.clone()).record(
        "key",
        &usage,
        &json!({
        "user_id":"u","key":"key","request_id":"request","model":"grok-4.7","pool":"grok",
        "provider":"xai","merchant_id":"merchant","status":200,"attempts":1,"created_at":1,
        "started_at_ms":1000,"duration_ms":5,"usage":usage,
        "billing":{"version":1,"state":"calculated","charge_microusd":amount}}),
        None,
    );
}
fn recovery(v: &Vault) -> Data {
    let mut d = v.lock().unwrap().clone();
    d.events.clear();
    d.consumption.clear();
    if let Some(state) = &mut d.receipt_relay {
        state.uploaded = state.settled.clone();
        state.retained.clear();
        d.sequence = state
            .settled
            .as_ref()
            .map_or(state.start_sequence, |h| h.through_seq);
    }
    d
}
fn reopen(dir: &Path, saved: Data) -> Arc<Vault> {
    let v = Vault::with_journal(dir, &KEY).unwrap();
    v.require_manager();
    assert!(!v.serving());
    v.initialize(saved).unwrap();
    v
}

#[test]
fn completed_receipts_survive_restart_without_management_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let v = create(dir.path(), "https://example.org");
    let saved = recovery(&v);
    let boot = v.boot_id.clone();
    record(&v, 10);
    record(&v, 20);
    drop(v);
    let next = reopen(dir.path(), saved);
    assert_ne!(next.boot_id, boot);
    let data = next.lock().unwrap();
    assert_eq!(data.events.len(), 2);
    assert_eq!(data.sequence, 2);
    assert_eq!(data.usage["key"].requests, 2);
    assert_eq!(crate::balances::available(&data, "u").unwrap(), 970);
}

#[tokio::test]
async fn refusal_totals_survive_upload_cleanup_and_restart_without_a_new_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (_cloud, url, task) = cloud().await;
    let v = create(dir.path(), &url);
    let saved = recovery(&v);
    record(&v, 0);
    let audit = v.lock().unwrap().events.back().unwrap()["event"].clone();
    let observer = crate::refusal::Observer::new(crate::Provider::OpenAI);
    for _ in 0..3 {
        observer.observe(
            crate::upstream::Protocol::Chat,
            &json!({"choices":[{"index":0,"delta":{"refusal":"declined"}}]}),
            true,
        );
    }
    let report = observer.report().unwrap();
    let ledger = crate::ledger::Ledger::new(v.clone());
    ledger.record(
        "key",
        &crate::ledger::Usage::default(),
        &audit,
        Some(&report),
    );
    ledger.record("key", &crate::ledger::Usage::default(), &audit, None); // ordinary failure is not a refusal
    assert_eq!(
        v.lock().unwrap().merchant_refusal_counts["merchant"]["u"],
        1
    );
    due(&v);
    v.publish_public(false).await.unwrap();
    assert!(v.lock().unwrap().events.is_empty());
    drop(ledger);
    drop(v);
    let next = reopen(dir.path(), saved);
    assert_eq!(
        next.lock().unwrap().merchant_refusal_counts["merchant"]["u"],
        1
    );
    assert!(next.lock().unwrap().events.is_empty());
    task.abort();
}

#[test]
fn wrong_key_row_deletion_and_concurrent_writer_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let v = create(dir.path(), "https://example.org");
    record(&v, 1);
    assert!(Vault::with_journal(dir.path(), &KEY).is_err());
    drop(v);
    assert!(Vault::with_journal(dir.path(), &[9; 32]).is_err());
    let db = Connection::open(dir.path().join("accounting.sqlite3")).unwrap();
    db.execute("DELETE FROM spool WHERE name LIKE 'e/%'", [])
        .unwrap();
    drop(db);
    assert!(Vault::with_journal(dir.path(), &KEY).is_err());
}

#[test]
fn failed_write_stops_new_admissions_and_recovery_uses_only_committed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let v = create(dir.path(), "https://example.org");
    let saved = recovery(&v);
    record(&v, 10);
    v.journal
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .db
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    record(&v, 20);
    assert!(!v.serving());
    assert_eq!(v.journal_status()["healthy"], false);
    drop(v);
    let next = reopen(dir.path(), saved);
    assert_eq!(next.lock().unwrap().sequence, 1);
    assert_eq!(next.lock().unwrap().consumption["u"], 10);
}

#[derive(Default)]
struct Cloud {
    calls: AtomicUsize,
    fail: AtomicUsize,
    bodies: Mutex<Vec<Value>>,
}
async fn upload(
    State(cloud): State<Arc<Cloud>>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    cloud.calls.fetch_add(1, Ordering::SeqCst);
    cloud.bodies.lock().unwrap().push(body.clone());
    if cloud.fail.load(Ordering::SeqCst) > 0 {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, Json(json!({})));
    }
    (
        axum::http::StatusCode::OK,
        Json(
            json!({"durable":true,"batch_hash":body["batch_hash"],"detail_state":"pending","settlement_acknowledged":false}),
        ),
    )
}
async fn cloud() -> (Arc<Cloud>, String, tokio::task::JoinHandle<()>) {
    let c = Arc::new(Cloud::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/streams/{stream}/batches/{number}", put(upload))
        .with_state(c.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (c, url, task)
}
fn due(v: &Vault) {
    let mut p = v.relay_publishing.lock().unwrap();
    p.last_attempt = Instant::now().checked_sub(Duration::from_secs(301));
}

#[tokio::test]
async fn failed_batch_survives_restart_and_waits_for_next_cycle_with_exact_same_signature() {
    let dir = tempfile::tempdir().unwrap();
    let (cloud, url, task) = cloud().await;
    let v = create(dir.path(), &url);
    let saved = recovery(&v);
    record(&v, 10);
    cloud.fail.store(1, Ordering::SeqCst);
    due(&v);
    assert!(v.publish_public(false).await.is_err());
    let first = cloud.bodies.lock().unwrap()[0].clone();
    // The removed 30-second retry must never be used, including after restart.
    v.relay_publishing.lock().unwrap().last_attempt =
        Instant::now().checked_sub(Duration::from_secs(31));
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 1);
    drop(v);
    let v = reopen(dir.path(), saved.clone());
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 1);
    record(&v, 20);
    cloud.fail.store(0, Ordering::SeqCst);
    due(&v);
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 3); // old frozen batch, then new tail
    assert_eq!(cloud.bodies.lock().unwrap()[1], first);
    assert!(v.lock().unwrap().events.is_empty());
    assert_eq!(v.lock().unwrap().consumption["u"], 30);
    let head = v
        .lock()
        .unwrap()
        .receipt_relay
        .as_ref()
        .unwrap()
        .uploaded
        .clone();
    drop(v);
    let v = reopen(dir.path(), saved);
    assert!(v.lock().unwrap().events.is_empty());
    assert_eq!(v.lock().unwrap().consumption["u"], 30);
    v.relay_settle(crate::receipt_relay::Settlement {
        boot_id: v.boot_id.clone(),
        revision: 1,
        through: head,
        balances: Some(BTreeMap::from([("u".into(), 970)])),
        balances_patch: None,
    })
    .unwrap();
    let saved = recovery(&v);
    drop(v);
    let v = reopen(dir.path(), saved);
    assert!(v.lock().unwrap().consumption.is_empty());
    assert_eq!(v.lock().unwrap().balances["u"], 970);
    task.abort();
}

#[tokio::test]
async fn one_cycle_splits_large_backlog_but_new_events_wait_until_next_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let (cloud, url, task) = cloud().await;
    let v = create(dir.path(), &url);
    for _ in 0..513 {
        record(&v, 1);
    }
    due(&v);
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 2);
    record(&v, 2);
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 2);
    assert_eq!(v.lock().unwrap().events.len(), 1);
    due(&v);
    v.publish_public(false).await.unwrap();
    assert_eq!(cloud.calls.load(Ordering::SeqCst), 3);
    task.abort();
}

#[tokio::test]
async fn cloud_commit_before_local_ack_and_manager_recovery_do_not_double_charge() {
    let dir = tempfile::tempdir().unwrap();
    let (cloud, url, task) = cloud().await;
    let v = create(dir.path(), &url);
    let mut saved = recovery(&v);
    record(&v, 10);
    // Freeze and durably store a batch, simulating a lost response after cloud
    // persistence. Management has independently ingested that exact batch.
    cloud.fail.store(1, Ordering::SeqCst);
    assert!(v.publish_public(true).await.is_err());
    let head = v
        .relay_publishing
        .lock()
        .unwrap()
        .pending
        .as_ref()
        .unwrap()
        .head
        .clone();
    saved.sequence = head.through_seq;
    saved.balances.insert("u".into(), 990);
    let r = saved.receipt_relay.as_mut().unwrap();
    r.uploaded = Some(head.clone());
    r.settled = Some(head.clone());
    drop(v);
    let v = reopen(dir.path(), saved);
    assert_eq!(
        crate::balances::available(&v.lock().unwrap(), "u").unwrap(),
        990
    );
    cloud.fail.store(0, Ordering::SeqCst);
    due(&v);
    v.publish_public(false).await.unwrap();
    let ack = crate::receipt_relay::Settlement {
        boot_id: v.boot_id.clone(),
        revision: 2,
        through: Some(head.clone()),
        balances: Some(BTreeMap::from([("u".into(), 990)])),
        balances_patch: None,
    };
    let encoded = serde_json::to_value(&ack).unwrap();
    v.relay_settle(ack).unwrap();
    v.relay_settle(serde_json::from_value(encoded).unwrap())
        .unwrap();
    assert_eq!(
        crate::balances::available(&v.lock().unwrap(), "u").unwrap(),
        990
    );
    assert!(v.lock().unwrap().events.is_empty());
    task.abort();
}

#[test]
fn crash_writer() {
    let Some(dir) = std::env::var_os("TEEROUTER_JOURNAL_CRASH_FIXTURE") else {
        return;
    };
    let v = create(Path::new(&dir), "https://example.org");
    record(&v, 17);
    std::fs::write(Path::new(&dir).join("committed"), b"ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn forced_process_kill_preserves_committed_wal_without_destructors() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "vault::journal::tests::crash_writer",
            "--nocapture",
        ])
        .env("TEEROUTER_JOURNAL_CRASH_FIXTURE", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dir.path().join("committed").exists() && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            panic!("crash fixture exited before commit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let committed = dir.path().join("committed").exists();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(committed, "crash fixture did not commit in time");
    let mut saved = baseline();
    saved.receipt_relay = Some(crate::receipt_relay::State {
        config: crate::receipt_relay::Config {
            url: "https://example.org".into(),
            upload_token: "t".repeat(64),
            interval_seconds: 300,
            user_ids: BTreeMap::from([("u".into(), "12".repeat(32))]),
        },
        start_sequence: 0,
        uploaded: None,
        settled: None,
        retained: Default::default(),
        balance_revision: 0,
        last_settlement_hash: None,
    });
    let v = reopen(dir.path(), saved);
    assert_eq!(v.lock().unwrap().consumption["u"], 17);
    assert_eq!(v.lock().unwrap().events.len(), 1);
}
