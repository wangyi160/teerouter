use super::*;
use crate::router::Engine;
use axum::{Json, Router, routing::any};
use reqwest::Method;

const ADMIN: &str = "platform-admin";
#[tokio::test]
async fn signed_checkpoint_requires_its_proof_hash_and_rejects_arbitrary_digest_attestation() {
    let r = root();
    let server = serve(crate::server::app_runtime(r)).await;
    let (status, first) = call(
        &server,
        Method::POST,
        "/admin/checkpoint/diff",
        ADMIN,
        json!({"sign_receipts":true}),
    )
    .await;
    assert_eq!(status, 200);
    let manifest: Value =
        serde_json::from_str(first["receipt_batch"]["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(manifest["record_count"], 0);
    let mut ack = json!({"boot_id":first["boot_id"],"generation":first["state"]["generation"],
        "sequence":first["state"]["sequence"],"balances_patch":{}});
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/checkpoint/ack",
            ADMIN,
            ack.clone()
        )
        .await
        .0,
        409
    );
    ack["receipt_batch_hash"] = first["receipt_batch"]["batch_hash"].clone();
    assert_eq!(
        call(&server, Method::POST, "/admin/checkpoint/ack", ADMIN, ack)
            .await
            .0,
        200
    );
    let (_, next) = call(
        &server,
        Method::POST,
        "/admin/checkpoint/diff",
        ADMIN,
        json!({}),
    )
    .await;
    let next_manifest: Value =
        serde_json::from_str(next["receipt_batch"]["manifest"].as_str().unwrap()).unwrap();
    assert_eq!(
        next_manifest["previous_batch_hash"],
        first["receipt_batch"]["batch_hash"]
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/receipts/evidence",
            "alice-key",
            json!({"nonce":"a".repeat(64)})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/receipts/evidence",
            ADMIN,
            json!({"nonce":"bad"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/receipts/evidence",
            ADMIN,
            json!({"nonce":"a".repeat(64),"digest":"b".repeat(64)})
        )
        .await
        .0,
        400
    );
}
#[tokio::test]
async fn checkpoint_diff_is_admin_only_and_survives_explicit_quiesce() {
    let r = root();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let endpoint = "/admin/checkpoint/diff";
    for key in ["alice-key", "invalid"] {
        assert_eq!(
            call(&server, Method::POST, endpoint, key, json!({}))
                .await
                .0,
            401
        );
    }
    let (code, first) = call(&server, Method::POST, endpoint, ADMIN, json!({})).await;
    assert_eq!(code, 200);
    assert!(first.get("state").is_some());
    let boot = first["boot_id"].clone();
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/quiesce",
            ADMIN,
            json!({"boot_id":boot,"quiescing":true})
        )
        .await
        .0,
        200
    );
    let (code, diff) = call(
        &server,
        Method::POST,
        endpoint,
        ADMIN,
        json!({"boot_id":boot,"base_id":first["checkpoint_id"]}),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(diff["changes"], json!([]));
    assert!(diff.get("state").is_none());
    let ack = json!({"boot_id":boot,"generation":first["state"]["generation"],
        "sequence":first["state"]["sequence"],"balances_patch":{}});
    for _ in 0..2 {
        assert_eq!(
            call(
                &server,
                Method::POST,
                "/admin/checkpoint/ack",
                ADMIN,
                ack.clone()
            )
            .await
            .0,
            200
        );
    }
    let mut ambiguous = ack;
    ambiguous["balances"] = json!({});
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/checkpoint/ack",
            ADMIN,
            ambiguous
        )
        .await
        .0,
        400
    );
}

#[tokio::test]
async fn unchanged_merchant_capsules_are_not_reencrypted_by_checkpoint_reads() {
    let r = root();
    let merchant = add(&r, &"a".repeat(64)).await;
    checkpoint_all(&r).await.unwrap();
    let before = r.engine().vault.checkpoint().unwrap();
    checkpoint_all(&r).await.unwrap();
    assert_eq!(r.engine().vault.checkpoint().unwrap(), before);
    {
        let engine = merchant.runtime.engine();
        let mut data = engine.vault.lock().unwrap();
        data.upstream_users.insert("alice".into());
        data.changed();
    }
    checkpoint_all(&r).await.unwrap();
    assert_ne!(
        r.engine().vault.checkpoint().unwrap()["state"]["merchants"],
        before["state"]["merchants"]
    );
}

#[tokio::test]
async fn explicit_quiesce_blocks_new_work_and_drains_shared_merchant_jobs() {
    let r = root();
    let token = "a".repeat(64);
    let merchant = add(&r, &token).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let body = json!({"boot_id":r.engine().vault.boot_id,"quiescing":true});
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/quiesce",
            "alice-key",
            body.clone()
        )
        .await
        .0,
        401
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/quiesce",
            ADMIN,
            json!({"boot_id":"old-boot","quiescing":true})
        )
        .await
        .0,
        409
    );
    let job = merchant.runtime.background_job().unwrap();
    let admitted = r.ingress.acquire(usize::MAX).unwrap();
    let (code, state) = call(&server, Method::POST, "/admin/quiesce", ADMIN, body).await;
    assert_eq!(code, 200);
    assert_eq!(state["drained"], false);
    assert!(merchant.runtime.background_job().is_none());
    let child = r.merchant_child(merchant.runtime.engine());
    assert!(child.background_job().is_none());
    for (method, path, key, body) in [
        (Method::GET, "/v1/models", "alice-key", Value::Null),
        (Method::GET, "/merchant/state", &token, Value::Null),
        (Method::POST, "/admin/apply", ADMIN, json!({})),
    ] {
        let (code, error) = call(&server, method, path, key, body).await;
        assert_eq!(code, 503);
        assert_eq!(error["error"]["code"], "router_draining");
    }
    assert_eq!(
        call(&server, Method::GET, "/health/live", "", Value::Null)
            .await
            .0,
        200
    );
    drop(job);
    assert_eq!(
        call(&server, Method::GET, "/admin/metrics", ADMIN, Value::Null)
            .await
            .1["lifecycle"]["drained"],
        false
    );
    drop(admitted);
    for _ in 0..100 {
        if call(&server, Method::GET, "/admin/metrics", ADMIN, Value::Null)
            .await
            .1["lifecycle"]["drained"]
            == true
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        call(&server, Method::GET, "/admin/metrics", ADMIN, Value::Null)
            .await
            .1["lifecycle"]["drained"],
        true
    );
    assert_eq!(
        call(
            &server,
            Method::GET,
            "/admin/checkpoint",
            ADMIN,
            Value::Null
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/quiesce",
            ADMIN,
            json!({"boot_id":r.engine().vault.boot_id,"quiescing":false})
        )
        .await
        .0,
        200
    );
    assert!(merchant.runtime.background_job().is_some());
    assert_eq!(
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .0,
        200
    );
}

#[path = "probe_tests.rs"]
mod account_probes;
#[path = "pool_model_tests.rs"]
mod pool_models;
#[path = "public_catalog_tests.rs"]
mod public_display;
#[path = "routing_tests.rs"]
mod routing_protocol;

#[tokio::test]
async fn price_source_is_platform_admin_only_and_blocks_static_overwrites() {
    let r = root();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for key in ["alice-key", "invalid"] {
        for (method, path) in [
            (Method::GET, "/admin/pricing/source"),
            (Method::PUT, "/admin/pricing/source"),
            (Method::POST, "/admin/pricing/refresh"),
        ] {
            assert_eq!(call(&server, method, path, key, json!({})).await.0, 401);
        }
    }
    let (_, source) = call(
        &server,
        Method::GET,
        "/admin/pricing/source",
        ADMIN,
        Value::Null,
    )
    .await;
    assert_eq!(source["enabled"], false);
    // Hold the job lock to make this test independent of the background worker timer.
    let _guard = r.pricing_sync.lock().await;
    let update = json!({"expected_revision":0,"enabled":true,"url":source["url"]});
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/admin/pricing/source",
            ADMIN,
            update.clone()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(&server, Method::PUT, "/admin/pricing/source", ADMIN, update)
            .await
            .0,
        409
    );
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/admin/pricing/refresh",
            ADMIN,
            json!({})
        )
        .await
        .1["error"]["code"],
        "pricing_sync_running"
    );
    let mut book =
        json!({"expected_revision":1,"providers":crate::pricing::book(&r).unwrap().providers});
    book["providers"][0]["version"] = json!(2);
    book["providers"][0]["models"][0]["rates"]["input"] = json!(1);
    assert_eq!(
        call(&server, Method::PUT, "/admin/pricing", ADMIN, book)
            .await
            .1["error"]["code"],
        "pricing_source_managed"
    );
    r.engine().vault.lock().unwrap().pricing_source.enabled = false;
    // Inject a deterministic download failure without contacting the network.
    r.engine().vault.lock().unwrap().pricing_source.url = "invalid://test".into();
    drop(_guard);
    let (status, queued) = call(
        &server,
        Method::POST,
        "/admin/pricing/refresh",
        ADMIN,
        json!({}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(queued["accepted"], true);
    assert_eq!(queued["running"], true);
    for _ in 0..100 {
        if r.pricing_sync.try_lock().is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let data = r.engine().vault.lock().unwrap().clone();
    assert_eq!(
        data.pricing_source.last_error.as_deref(),
        Some("invalid_pricing_source")
    );
    assert_eq!(data.official_pricing.revision, 1);
}

#[tokio::test]
async fn public_price_book_is_read_only_and_independent_of_account_inventory() {
    let r = root();
    #[cfg(feature = "workbuddy-lab")]
    r.engine()
        .vault
        .lock()
        .unwrap()
        .official_pricing
        .providers
        .push(
            serde_json::from_value(
                json!({"provider":"workbuddy","version":1,"source":"local-test:workbuddy",
            "checked_at":1,"models":[{"model":"test-only","rates":{"input":0,"output":0}}]}),
            )
            .unwrap(),
        );
    let s = serve(crate::server::app_runtime(r)).await;
    for key in ["alice-key", "bob-key"] {
        let (status, prices) = call(&s, Method::GET, "/v1/pricing", key, Value::Null).await;
        assert_eq!(status, 200);
        assert_eq!(prices["currency"], "USD");
        assert_eq!(prices["unit"], "micro_usd_per_million_tokens");
        assert_eq!(prices["revision"], 1);
        assert_eq!(prices["providers"].as_array().unwrap().len(), 1);
        assert_eq!(prices["providers"][0]["models"][0]["model"], "model-test");
        assert_eq!(
            prices["providers"][0]["models"][0]["rates"]["input"],
            1000000
        );
        assert_eq!(
            call(&s, Method::GET, "/v1/models", key, Value::Null)
                .await
                .1["data"],
            json!([])
        );
        assert_eq!(
            call(&s, Method::PUT, "/v1/pricing", key, json!({})).await.0,
            405
        );
        assert_eq!(
            call(&s, Method::GET, "/admin/pricing", key, Value::Null)
                .await
                .0,
            401
        );
    }
    assert_eq!(
        call(&s, Method::GET, "/v1/pricing", "invalid", Value::Null)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn pool_multiplier_quotes_are_authorized_versioned_and_frozen() {
    let r = root();
    let token = "a".repeat(64);
    let live = add(&r, &token).await;
    let mut change = update(2);
    change["pool_pricing"][0]["multiplier_millionths"] = json!(200000);
    apply(&r, &token, serde_json::from_value(change).unwrap())
        .await
        .unwrap();
    let _mock = fixture(&live, completion()).await;
    let frozen = routing::selections(&r, "alice").await.unwrap().remove(0);
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let (_, pools) = call(&server, Method::GET, "/v1/pools", "alice-key", Value::Null).await;
    assert_eq!(pools["data"][0]["multiplier_millionths"], 200000);
    let (_, models) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    let quote = &models["data"][0]["router_pricing"]["rates"][0];
    assert_eq!(quote["basis"], "official_pool_multiplier");
    assert_eq!(quote["input"], 1000000);
    assert_eq!(quote["multiplier_millionths"], 200000);
    let mut update =
        json!({"expected_revision":1,"providers":crate::pricing::book(&r).unwrap().providers});
    update["providers"][0]["version"] = json!(2);
    update["providers"][0]["models"][0]["rates"]["input"] = json!(3000000);
    for key in ["alice-key", token.as_str()] {
        assert_eq!(
            call(&server, Method::PUT, "/admin/pricing", key, update.clone())
                .await
                .0,
            401
        );
    }
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/admin/pricing",
            ADMIN,
            update.clone()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/admin/pricing",
            ADMIN,
            update.clone()
        )
        .await
        .0,
        409
    );
    assert_eq!(
        crate::pricing::quotes(&frozen.settings, &frozen.engine.config, &frozen.price_book)[0]
            .price
            .input,
        1000000
    );
    update["expected_revision"] = json!(2);
    update["providers"][0]["version"] = json!(1);
    assert_eq!(
        call(&server, Method::PUT, "/admin/pricing", ADMIN, update)
            .await
            .0,
        400
    );
    let id = crypto::identity(&token);
    let body =
        json!({"model":format!("{id}/model-test"),"messages":[{"role":"user","content":"hello"}]});
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-router-official-price-revision", "1".parse().unwrap());
    let result = routing::execute(
        &r,
        r.engine(),
        0,
        crate::router::Protocol::Chat,
        &format!("{id}/model-test"),
        body.clone(),
        headers,
        "stale-book".into(),
    )
    .await;
    assert!(matches!(result,Err(e) if e.status==409));
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body
        )
        .await
        .0,
        200
    );
    let data = r.engine().vault.lock().unwrap().clone();
    let event = serde_json::to_value(data.events.back().unwrap()).unwrap();
    let quote = &event["event"]["merchant_pricing"];
    assert_eq!(quote["input"], 3000000, "{event}");
    assert_eq!(quote["multiplier_millionths"], 200000);
    assert_eq!(quote["official_price_revision"], 2);
    let bill = &event["event"]["billing"];
    assert_eq!(bill["calculator"], "chatgpt_v1");
    assert_eq!(bill["state"], "calculated", "{event}");
    assert_eq!(bill["official_cost_microusd"], 40);
    assert_eq!(bill["charge_microusd"], 8);
    let merchant_events = live.runtime.engine().vault.lock().unwrap().events.clone();
    let merchant_event = serde_json::to_value(merchant_events.back().unwrap()).unwrap();
    assert_eq!(merchant_event["event"]["billing"], *bill);
}

#[tokio::test]
async fn legacy_prices_migrate_only_when_the_whole_pool_has_one_exact_multiplier() {
    let r = root();
    let token = "a".repeat(64);
    let live = add(&r, &token).await;
    let config = live.runtime.engine().config.clone();
    let book = crate::pricing::book(&r).unwrap();
    let mut settings = live.settings.lock().unwrap().clone();
    settings.pool_pricing.clear();
    settings.prices = vec![
        serde_json::from_value(
            json!({"pool":"official","model":"model-test","input":200000,"output":400000}),
        )
        .unwrap(),
    ];
    let mut invalid = settings.clone();
    invalid.prices[0].output = 500000;
    assert!(crate::pricing::migrate(&mut invalid, &config, &book).is_err());
    assert!(crate::pricing::migrate(&mut settings.clone(), &config, &Default::default()).is_err());
    crate::pricing::migrate(&mut settings, &config, &book).unwrap();
    assert!(settings.prices.is_empty());
    assert_eq!(settings.pool_pricing[0].multiplier_millionths, 200000);
    let mut bad = update(2);
    bad["prices"] = json!([{"pool":"official","model":"model-test","input":0,"output":0}]);
    assert!(
        apply(&r, &token, serde_json::from_value(bad).unwrap())
            .await
            .is_err()
    );
    settings.pool_pricing.push(settings.pool_pricing[0].clone());
    assert!(crate::pricing::validate_pools(&settings, &config).is_err());
    settings.pool_pricing.pop();
    settings.pool_pricing[0].multiplier_millionths = 1_000_000_000_000_001;
    assert!(crate::pricing::validate_pools(&settings, &config).is_err());
}
fn root() -> Arc<Runtime> {
    let config: Config = serde_json::from_value(json!({"server":{"allow_empty_routes":true,"admin_key_ref":"ADMIN"},"users":[{"id":"alice"},{"id":"bob"}],"keys":[{"name":"alice-key","user_id":"alice","key_ref":"ALICE"},{"name":"bob-key","user_id":"bob","key_ref":"BOB"}],"models":[]})).unwrap();
    let runtime = Runtime::new(
        Engine::new(config, |k| {
            Some(
                match k {
                    "ADMIN" => ADMIN,
                    "ALICE" => "alice-key",
                    _ => "bob-key",
                }
                .into(),
            )
        })
        .unwrap(),
    );
    runtime.engine().vault.lock().unwrap().official_pricing = serde_json::from_value(json!({"revision":1,"providers":[{"provider":"openai","version":1,"source":"https://developers.openai.com/api/docs/pricing","checked_at":1,"models":[{"model":"model-test","rates":{"input":1000000,"output":2000000}}]}]})).unwrap();
    runtime
}
fn registration(token: &str) -> Registration {
    serde_json::from_value(json!({"id":crypto::identity(token),"credential_hash":crypto::digest(token),"name":"Shop","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"users":["alice"]})).unwrap()
}
fn update(revision: u64) -> Value {
    json!({"expected_revision":revision,"max_concurrent":16,"pools":[{"name":"official","provider":"openai","deployments":[{"id":"account","provider":"openai","api_key_ref":"upstream","enabled":false}]}],"customers":[],"pool_pricing":[{"pool":"official","multiplier_millionths":1000000}],"secrets":{"upstream":"MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL"}})
}
async fn add(root: &Runtime, token: &str) -> Arc<Live> {
    register(root, registration(token), ADMIN).await.unwrap();
    unlock(root, token, 0).await.unwrap();
    apply(root, token, serde_json::from_value(update(1)).unwrap())
        .await
        .unwrap();
    live(root, &crypto::identity(token)).unwrap()
}
struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(app: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { url, task }
}
async fn call(s: &Server, method: Method, path: &str, token: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
        .request(method, format!("{}{path}", s.url))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let value = r.json().await.unwrap();
    (status, value)
}
// HTTP fixture is injected only inside this cfg(test) module, never through a merchant API.
async fn fixture(live: &Live, reply: Value) -> Server {
    let mock = serve(
        Router::new()
            .route(
                "/v1/models",
                any(|| async { Json(json!({"data":[{"id":"model-test"}]})) }),
            )
            .fallback(any(move || {
                let r = reply.clone();
                async move { Json(r) }
            })),
    )
    .await;
    let mut config = live.runtime.engine().config.clone();
    let d = &mut config.models[0].deployments[0];
    d.api_base = Some(format!("{}/v1", mock.url));
    d.proxy = Some(mock.url.clone());
    d.enabled = true;
    live.runtime
        .apply(
            Update {
                config: Some(serde_json::to_value(config).unwrap()),
                ..Update::default()
            },
            &live.admin,
        )
        .await
        .unwrap();
    mock
}
fn completion() -> Value {
    json!({"id":"reply","model":"model-test","choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"Provider declined this request"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
}

#[tokio::test]
async fn platform_rejects_upstream_configuration_and_unowned_requests() {
    let runtime = root();
    let server = serve(crate::server::app_runtime(runtime.clone())).await;
    for body in [
        json!({"config":{"models":[{"name":"legacy","provider":"openai","deployments":[{"id":"a","provider":"openai","api_key_ref":"key","enabled":false}]}]}}),
        json!({"secrets":{"unowned-upstream":"private"}}),
    ] {
        let (status, result) = call(&server, Method::POST, "/admin/apply", ADMIN, body).await;
        assert_eq!(status, 400, "{result}");
        assert_eq!(result["error"]["code"], "merchant_ownership_required");
    }
    assert!(runtime.engine().config.models.is_empty());
    for path in [
        "/admin/accounts",
        "/admin/accounts/a/credential",
        "/admin/models",
    ] {
        assert_eq!(
            call(&server, Method::GET, path, ADMIN, Value::Null).await.0,
            404
        );
    }
    let (status, result) = call(
        &server,
        Method::POST,
        "/v1/chat/completions",
        "alice-key",
        json!({"model":"model-test","messages":[{"role":"user","content":"hello"}]}),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(result["error"]["code"], "merchant_required");
    assert!(
        call(&server, Method::GET, "/v1/models", "alice-key", Value::Null)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        call(&server, Method::GET, "/v1/pools", "alice-key", Value::Null)
            .await
            .1["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let token = "a".repeat(64);
    let merchant = add(&runtime, &token).await;
    let _upstream = fixture(&merchant, completion()).await;
    assert_eq!(call(&server,Method::POST,"/v1/chat/completions","alice-key",
        json!({"model":format!("{}/model-test",crypto::identity(&token)),"messages":[{"role":"user","content":"hello"}]})).await.0,200);
}

#[tokio::test]
async fn unified_platform_rpm_is_shared_across_merchants() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let la = add(&r, &a).await;
    let lb = add(&r, &b).await;
    let _ma = fixture(&la, completion()).await;
    let _mb = fixture(&lb, completion()).await;
    let mut config = r.engine().config.clone();
    config.keys[0].rpm = 1;
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            ..Update::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let server = serve(crate::server::app_runtime(r)).await;
    for (token, expected) in [(&a, 200), (&b, 429)] {
        let body = json!({"model":format!("{}/model-test",crypto::identity(token)),"messages":[{"role":"user","content":"hello"}]});
        let (status, result) = call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body,
        )
        .await;
        assert_eq!(status, expected, "{result}");
    }
}

#[tokio::test]
async fn merchant_concurrency_cap_is_enforced_and_released() {
    let r = root();
    let a = "a".repeat(64);
    let l = add(&r, &a).await;
    let _mock = fixture(&l, completion()).await;
    let mut registration = registration(&a);
    registration.max_concurrent = 1;
    register(&r, registration, ADMIN).await.unwrap();
    let held = l.gate.acquire(1).unwrap();
    let server = serve(crate::server::app_runtime(r)).await;
    let body = json!({"model":format!("{}/model-test",crypto::identity(&a)),"messages":[{"role":"user","content":"hello"}]});
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body.clone()
        )
        .await
        .0,
        429
    );
    drop(held);
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn namespaced_model_acl_accepts_only_its_merchant() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let la = add(&r, &a).await;
    let lb = add(&r, &b).await;
    let _ma = fixture(&la, completion()).await;
    let _mb = fixture(&lb, completion()).await;
    let mut config = r.engine().config.clone();
    config.keys[0].models = vec![format!("{}/model-test", crypto::identity(&a))];
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            ..Update::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let server = serve(crate::server::app_runtime(r)).await;
    for (token, expected) in [(&a, 200), (&b, 403)] {
        let (status, result) = call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            json!({"model":format!("{}/model-test",crypto::identity(token)),"messages":[{"role":"user","content":"hello"}]}),
        )
        .await;
        assert_eq!(status, expected, "{result}");
    }
}

#[cfg(feature = "workbuddy-lab")]
#[tokio::test]
async fn workbuddy_needs_merchant_permission_and_cannot_be_an_official_fallback() {
    let r = root();
    let a = "a".repeat(64);
    add(&r, &a).await;
    let mut v = update(2);
    v["pools"][0]["provider"] = "workbuddy".into();
    v["pools"][0]["deployments"][0]["provider"] = "workbuddy".into();
    assert!(
        apply(&r, &a, serde_json::from_value(v.clone()).unwrap())
            .await
            .is_err()
    );
    let mut reg = registration(&a);
    reg.allow_test = true;
    register(&r, reg, ADMIN).await.unwrap();
    apply(&r, &a, serde_json::from_value(v).unwrap())
        .await
        .unwrap();
    let l = live(&r, &crypto::identity(&a)).unwrap();
    let mut config = l.runtime.engine().config.clone();
    let mut official = config.models[0].clone();
    official.name = "other".into();
    official.provider = Some(crate::Provider::OpenAI);
    official.deployments[0].id = "other".into();
    official.deployments[0].provider = crate::Provider::OpenAI;
    official.fallbacks = vec!["official".into()];
    config.models.push(official);
    assert!(config.validate().is_err());
}

#[tokio::test]
async fn merchant_credentials_are_separate_and_root_backups_are_opaque() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    add(&r, &a).await;
    add(&r, &b).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for (token, path) in [
        (&a, "/admin/checkpoint"),
        (&a, "/admin/accounts/account/credential"),
        (&a, "/v1/models"),
        (&ADMIN.to_owned(), "/merchant/state"),
        (&"alice-key".to_owned(), "/merchant/state"),
    ] {
        assert!(call(&server, Method::GET, path, token, Value::Null).await.0 >= 400);
    }
    let (status, value) = call(&server, Method::GET, "/merchant/state", &a, Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(value["merchant"]["id"], crypto::identity(&a));
    assert!(!value.to_string().contains("MERCHANT_PRIVATE"));
    assert!(!value.to_string().contains(&crypto::identity(&b)));
    checkpoint_all(&r).await.unwrap();
    let serialized = serde_json::to_string(&*r.engine().vault.lock().unwrap()).unwrap();
    assert!(!serialized.contains("MERCHANT_PRIVATE"));
    assert!(!serialized.contains(&a));
    assert!(serialized.contains("ciphertext"));
}

#[tokio::test]
async fn updates_cannot_select_foreign_users_internal_secrets_or_untrusted_endpoints() {
    let r = root();
    let a = "a".repeat(64);
    add(&r, &a).await;
    let mut cases = vec![];
    let mut v = update(2);
    v["customers"] = json!([{"user_id":"bob","enabled":true,"models":[],"max_concurrent":16}]);
    cases.push(v);
    let mut v = update(2);
    v["secrets"]["merchant_internal_admin"] = "overwrite".into();
    cases.push(v);
    let mut v = update(2);
    v["pools"][0]["deployments"][0]["api_key_ref"] = "merchant_internal_admin".into();
    cases.push(v);
    let mut v = update(2);
    v["pools"][0]["deployments"][0]["api_base"] = "https://third-party.example/v1".into();
    cases.push(v);
    let mut v = update(2);
    v["pools"][0]["fallbacks"] = json!(["another-merchant"]);
    cases.push(v);
    for v in cases {
        assert!(
            apply(&r, &a, serde_json::from_value(v).unwrap())
                .await
                .is_err()
        );
    }
    assert_eq!(
        live(&r, &crypto::identity(&a))
            .unwrap()
            .settings
            .lock()
            .unwrap()
            .revision,
        2
    );
    assert!(
        apply(&r, &a, serde_json::from_value(update(1)).unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn recovery_rejects_tampering_wrong_identity_and_revision_rollback() {
    let r = root();
    let a = "a".repeat(64);
    add(&r, &a).await;
    let id = crypto::identity(&a);
    let backup = records(&r).unwrap()[0].backup.clone().unwrap();
    assert!(crypto::open(&id, &crypto::key(&"b".repeat(64)), &backup).is_err());
    assert!(crypto::open("another-merchant", &crypto::key(&a), &backup).is_err());
    let mut bad = backup.clone();
    bad.ciphertext.replace_range(0..4, "AAAA");
    assert!(crypto::open(&id, &crypto::key(&a), &bad).is_err());
    r.merchants.live.lock().unwrap().clear();
    assert!(unlock(&r, &a, 3).await.is_err());
    let restored = unlock(&r, &a, 2).await.unwrap();
    assert_eq!(restored["settings"]["revision"], 2);
    assert_eq!(restored["settings"]["customers"], json!([]));
    assert_eq!(
        live(&r, &id)
            .unwrap()
            .runtime
            .engine()
            .vault
            .snapshot()
            .unwrap()
            .bundle
            .secrets["upstream"],
        "MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL"
    );
}

#[tokio::test]
async fn unified_key_routes_two_merchants_and_audits_independent_queues() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let la = add(&r, &a).await;
    let lb = add(&r, &b).await;
    let _ma = fixture(&la, completion()).await;
    let _mb = fixture(&lb, completion()).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let (_, models) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "chatgpt/model-test");
    assert_eq!(
        models["data"][0]["router_pools"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        models["data"][0]["router_merchants"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (_, bob) = call(&server, Method::GET, "/v1/models", "bob-key", Value::Null).await;
    assert!(bob["data"].as_array().unwrap().is_empty());
    for token in [&a, &b] {
        let body = json!({"model":format!("{}/model-test",crypto::identity(token)),"messages":[{"role":"user","content":"REQUEST_BODY_MUST_NOT_BE_SAVED"}]});
        let (status, result) = call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            body,
        )
        .await;
        assert_eq!(status, 200, "{result}");
    }
    let (_, global) = call(&server, Method::GET, "/admin/refusals", ADMIN, Value::Null).await;
    assert_eq!(global["data"].as_array().unwrap().len(), 2);
    assert_eq!(global["last_id"], 2);
    assert!(global["data"][0]["merchant_id"].is_string());
    assert!(
        !global
            .to_string()
            .contains("REQUEST_BODY_MUST_NOT_BE_SAVED")
    );
    for token in [&a, &b] {
        let (_, usage) = call(&server, Method::GET, "/merchant/usage", token, Value::Null).await;
        assert_eq!(usage["data"].as_array().unwrap().len(), 1);
        assert_eq!(usage["data"][0]["event"]["merchant_price_version"], 2);
        assert_eq!(
            usage["data"][0]["event"]["merchant_pricing"]["input"],
            1000000
        );
    }
    assert_eq!(
        r.engine().vault.lock().unwrap().usage["alice-key"].requests,
        2
    );
    let (status, _) = call(
        &server,
        Method::DELETE,
        "/merchant/refusals?through_id=1",
        &a,
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        la.runtime
            .engine()
            .vault
            .lock()
            .unwrap()
            .refusal_queue
            .events
            .len(),
        0
    );
    assert_eq!(
        lb.runtime
            .engine()
            .vault
            .lock()
            .unwrap()
            .refusal_queue
            .events
            .len(),
        1
    );
    assert_eq!(
        r.engine().vault.lock().unwrap().refusal_queue.events.len(),
        2
    );
}

#[tokio::test]
async fn explicit_selection_prices_and_revocation_fail_closed() {
    let r = root();
    let a = "a".repeat(64);
    let l = add(&r, &a).await;
    let _mock = fixture(&l, completion()).await;
    let id = crypto::identity(&a);
    let request = || json!({"model":format!("{id}/model-test"),"messages":[]});
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-router-pool",
        format!("{}.official", crypto::identity(&"b".repeat(64)))
            .parse()
            .unwrap(),
    );
    assert!(
        routing::execute(
            &r,
            r.engine(),
            0,
            crate::router::Protocol::Chat,
            &format!("{id}/model-test"),
            request(),
            headers,
            "one".into()
        )
        .await
        .is_err()
    );
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-router-price-version", "1".parse().unwrap());
    let result = routing::execute(
        &r,
        r.engine(),
        0,
        crate::router::Protocol::Chat,
        &format!("{id}/model-test"),
        request(),
        headers,
        "two".into(),
    )
    .await;
    assert!(matches!(result,Err(e) if e.status==409));
    let mut reg = registration(&a);
    reg.enabled = false;
    register(&r, reg, ADMIN).await.unwrap();
    assert!(routing::selections(&r, "alice").await.unwrap().is_empty());
    assert!(authenticate(&r, &a).is_err());
}

#[tokio::test]
async fn catalog_acl_and_price_lists_hide_other_merchant_pools() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let la = add(&r, &a).await;
    let lb = add(&r, &b).await;
    let _ma = fixture(&la, completion()).await;
    let _mb = fixture(&lb, completion()).await;
    let mut config = r.engine().config.clone();
    config.keys[0].models = vec![format!("{}.official/*", crypto::identity(&a))];
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            ..Update::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let server = serve(crate::server::app_runtime(r)).await;
    let (_, catalog) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert_eq!(catalog["data"].as_array().unwrap().len(), 1);
    assert!(!catalog.to_string().contains(&crypto::identity(&b)));
    let (_, pools) = call(&server, Method::GET, "/v1/pools", "alice-key", Value::Null).await;
    assert_eq!(pools["data"].as_array().unwrap().len(), 1);
    let (_, merchants) = call(
        &server,
        Method::GET,
        "/v1/merchants",
        "alice-key",
        Value::Null,
    )
    .await;
    let other = merchants["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == crypto::identity(&b))
        .unwrap();
    assert!(other["prices"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn customers_are_private_until_upstream_use_and_remain_visible_after_ack_and_restore() {
    let r = root();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let la = add(&r, &a).await;
    add(&r, &b).await;
    let mut reg = registration(&a);
    reg.users.insert("bob".into());
    register(&r, reg.clone(), ADMIN).await.unwrap();
    let mut config = update(2);
    config["customer_defaults"] = json!({"max_concurrent":3,"rpm":7});
    apply(&r, &a, serde_json::from_value(config).unwrap())
        .await
        .unwrap();
    let _mock = fixture(&la, completion()).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    // Discovery may build internal admission entries; neither user becomes visible.
    for key in ["alice-key", "bob-key"] {
        for path in ["/v1/models", "/v1/pools", "/v1/merchants"] {
            assert_eq!(
                call(&server, Method::GET, path, key, Value::Null).await.0,
                200
            );
        }
    }
    let state = state(&r, &reg).unwrap();
    assert!(state["merchant"].get("users").is_none());
    assert_eq!(state["settings"]["customers"], json!([]));
    assert!(!state.to_string().contains("bob"));
    // Guessing a preauthorized but unused ID has the same denial as a foreign ID.
    for id in ["bob", "unknown"] {
        let mut v = update(3);
        v["customers"] = json!([{"user_id":id,"enabled":true,"models":[],"max_concurrent":16}]);
        assert_eq!(
            apply(&r, &a, serde_json::from_value(v).unwrap())
                .await
                .unwrap_err()
                .status,
            403
        );
    }
    let id = crypto::identity(&a);
    let bad = json!({"model":format!("{id}/model-test"),"messages":"invalid local request"});
    assert!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "bob-key",
            bad
        )
        .await
        .0 >= 400
    );
    let (_, usage) = call(&server, Method::GET, "/merchant/usage", &a, Value::Null).await;
    assert!(!usage.to_string().contains("bob"));
    let request =
        json!({"model":format!("{id}/model-test"),"messages":[{"role":"user","content":"Hello"}]});
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            request.clone()
        )
        .await
        .0,
        200
    );
    let visible = super::state(&r, &reg).unwrap();
    assert_eq!(
        visible["settings"]["customers"].as_array().unwrap().len(),
        1
    );
    assert_eq!(visible["settings"]["customers"][0]["user_id"], "alice");
    assert_eq!(visible["settings"]["customers"][0]["max_concurrent"], 3);
    assert_eq!(visible["settings"]["customers"][0]["rpm"], 7);
    assert_eq!(
        super::state(&r, &registration(&b)).unwrap()["settings"]["customers"],
        json!([])
    );
    // A stale empty customer list cannot erase the explicit stop, even on unrelated edits.
    let mut v = update(3);
    v["customers"] = visible["settings"]["customers"].clone();
    v["customers"][0]["enabled"] = false.into();
    apply(&r, &a, serde_json::from_value(v).unwrap())
        .await
        .unwrap();
    apply(&r, &a, serde_json::from_value(update(4)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/v1/chat/completions",
            "alice-key",
            request
        )
        .await
        .0,
        403
    );
    let (_, usage) = call(&server, Method::GET, "/merchant/usage", &a, Value::Null).await;
    assert_eq!(
        call(
            &server,
            Method::POST,
            "/merchant/usage/ack",
            &a,
            json!({"instance_id":usage["instance_id"],"through_seq":usage["last_seq"]})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        call(
            &server,
            Method::DELETE,
            "/merchant/refusals?through_id=1",
            &a,
            Value::Null
        )
        .await
        .0,
        200
    );
    checkpoint_all(&r).await.unwrap();
    r.merchants.live.lock().unwrap().clear();
    let restored = unlock(&r, &a, 5).await.unwrap();
    assert_eq!(
        restored["settings"]["customers"].as_array().unwrap().len(),
        1
    );
    assert_eq!(restored["settings"]["customers"][0]["user_id"], "alice");
    assert_eq!(restored["settings"]["customers"][0]["enabled"], false);
    // Revoking platform access does not erase the merchant's existing relationship.
    reg.users.clear();
    register(&r, reg.clone(), ADMIN).await.unwrap();
    assert_eq!(
        super::state(&r, &reg).unwrap()["settings"]["customers"][0]["user_id"],
        "alice"
    );
    assert!(
        routing::selections(&r, "alice")
            .await
            .unwrap()
            .iter()
            .all(|s| s.item.id != id)
    );
}

#[tokio::test]
async fn legacy_customer_migration_uses_completed_usage_not_preauthorization() {
    let r = root();
    let token = "a".repeat(64);
    let live = add(&r, &token).await;
    let engine = live.runtime.engine();
    customers::ensure(&live, "alice").await.unwrap();
    customers::ensure(&live, "bob").await.unwrap();
    let mut data = engine.vault.lock().unwrap();
    data.usage.insert(
        "alice".into(),
        crate::ledger::Usage {
            requests: 2,
            errors: 1,
            ..Default::default()
        },
    );
    data.usage.insert(
        "bob".into(),
        crate::ledger::Usage {
            requests: 1,
            errors: 1,
            ..Default::default()
        },
    );
    assert!(data.upstream_users.is_empty());
    customers::restore(&mut data);
    assert_eq!(data.upstream_users, BTreeSet::from(["alice".into()]));
}
