use super::*;

async fn admin(g: &Running, method: &str, path: &str, body: Value) -> reqwest::Response {
    client()
        .request(method.parse().unwrap(), format!("{}{path}", g.url))
        .bearer_auth("admin-secret")
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn generate(g: &Running, key: &str, stream: bool) -> reqwest::Response {
    client()
        .post(format!("{}/v1/responses", g.url))
        .bearer_auth(key)
        .json(&json!({"model":"chat","input":"hello","stream":stream}))
        .send()
        .await
        .unwrap()
}
fn user(id: &str, key: &str) -> Value {
    json!({"id":id,"max_concurrent":1,"keys":[{"name":format!("{id}-key"),"key":key}]})
}

#[tokio::test]
async fn managed_balance_blocks_upstream_but_keeps_catalog_access_and_accepts_topup() {
    let mock = mock(vec![Reply::json(native(Provider::OpenAI))]).await;
    let engine = engine(configuration(&mock));
    engine.vault.require_manager();
    let initial = engine.vault.lock().unwrap().clone();
    engine.vault.initialize(initial).unwrap();
    let g = run_fixture(engine.clone()).await;
    for (balances, status, code) in [
        (json!({}), 503, "balance_unavailable"),
        (json!({"test-user":0}), 402, "insufficient_balance"),
        (json!({"test-user":-1}), 402, "insufficient_balance"),
    ] {
        let checkpoint = engine.vault.checkpoint().unwrap();
        let response = admin(
            &g,
            "POST",
            "/admin/checkpoint/ack",
            json!({
                "boot_id":checkpoint["boot_id"], "generation":checkpoint["state"]["generation"],
                "sequence":checkpoint["state"]["sequence"], "balances":balances
            }),
        )
        .await;
        assert_eq!(response.status(), 200);
        let response = generate(&g, "client-secret", false).await;
        assert_eq!(response.status(), status);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            code
        );
        assert!(mock.calls.lock().unwrap().is_empty());
    }
    assert_eq!(
        client()
            .get(format!("{}/v1/models", g.url))
            .bearer_auth("client-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let checkpoint = engine.vault.checkpoint().unwrap();
    assert_eq!(
        admin(
            &g,
            "POST",
            "/admin/checkpoint/ack",
            json!({
                "boot_id":checkpoint["boot_id"], "generation":checkpoint["state"]["generation"],
                "sequence":checkpoint["state"]["sequence"], "balances":{"test-user":100}
            })
        )
        .await
        .status(),
        200
    );
    assert_eq!(generate(&g, "client-secret", false).await.status(), 200);
    assert_eq!(mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn balance_policy_hot_update_validates_and_restores_with_checkpoint() {
    let mock = mock(vec![]).await;
    let engine = engine(configuration(&mock));
    engine.vault.require_manager();
    let initial = engine.vault.lock().unwrap().clone();
    engine.vault.initialize(initial).unwrap();
    let checkpoint = engine.vault.checkpoint().unwrap();
    engine
        .vault
        .acknowledge(
            &engine.vault.boot_id,
            checkpoint["state"]["generation"].as_u64().unwrap(),
            checkpoint["state"]["sequence"].as_u64().unwrap(),
            std::collections::BTreeMap::from([("test-user".into(), 2_000_000)]),
        )
        .unwrap();
    let g = run_fixture(engine.clone()).await;
    let current: Value = admin(&g, "GET", "/admin/config", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    let u: Value = admin(&g, "GET", "/admin/users/test-user", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(u["user"]["effective_max_concurrent"], 2);
    let policy = json!({"tiers":[{"up_to_microusd":3_000_000,"max_concurrent":1}]});
    assert_eq!(
        admin(
            &g,
            "PATCH",
            "/admin/config",
            json!({"expected_revision":current["revision"], "patch":{"balance_concurrency":policy}})
        )
        .await
        .status(),
        200
    );
    let u: Value = admin(&g, "GET", "/admin/users/test-user", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(u["user"]["effective_max_concurrent"], 1);
    let changed: Value = admin(&g, "GET", "/admin/config", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        admin(
            &g,
            "PATCH",
            "/admin/config",
            json!({"expected_revision":current["revision"], "patch":{"balance_concurrency":policy}})
        )
        .await
        .status(),
        409
    );
    assert_eq!(admin(&g, "PATCH", "/admin/config", json!({"expected_revision":changed["revision"], "patch":{"balance_concurrency":{"tiers":[]}}})).await.status(), 400);
    let saved = engine.vault.checkpoint().unwrap();
    assert_eq!(
        saved["state"]["bundle"]["config"]["balance_concurrency"],
        policy
    );
    let restored: crate::vault::Data = serde_json::from_value(saved["state"].clone()).unwrap();
    let next = crate::vault::Vault::memory().unwrap();
    next.require_manager();
    next.initialize(restored).unwrap();
    let data = next.lock().unwrap();
    let bundle = data.bundle.as_ref().unwrap();
    assert_eq!(
        crate::balances::concurrency_limit(
            &data,
            &bundle.config.users[0],
            &bundle.config.balance_concurrency
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn user_crud_is_atomic_admin_only_and_contains_no_money() {
    let mock = mock(vec![]).await;
    let g = gateway(configuration(&mock)).await;
    assert_eq!(
        post(&g, "/admin/users", user("a", "a-secret"))
            .await
            .status(),
        401
    );
    let mut u = user("a", "a-secret");
    u["balance"] = json!(100);
    assert_eq!(admin(&g, "POST", "/admin/users", u).await.status(), 400);
    assert_eq!(
        admin(&g, "GET", "/admin/users/a", Value::Null)
            .await
            .status(),
        404
    );
    assert_eq!(
        admin(&g, "POST", "/admin/users", user("a", "a-secret"))
            .await
            .status(),
        200
    );
    assert_eq!(
        admin(&g, "POST", "/admin/users", user("a", "a-secret"))
            .await
            .status(),
        409
    );
    let res = admin(&g, "GET", "/admin/users/a", Value::Null).await;
    assert_eq!(res.headers()["cache-control"], "no-store");
    let before: Value = res.json().await.unwrap();
    assert_eq!(before["user"]["keys"][0]["user_id"], "a");
    assert_eq!(before["user"]["keys"][0]["key"], "a-secret");
    for bad in [
        json!({"max_concurrent":0}),
        json!({"keys":[{"name":"broken"}]}),
        json!({"keys":[{"name":"user","key":"stolen"}]}),
        json!({"expected_revision":0,"name":"stale"}),
    ] {
        assert!(
            admin(&g, "PUT", "/admin/users/a", bad)
                .await
                .status()
                .is_client_error()
        );
        let after: Value = admin(&g, "GET", "/admin/users/a", Value::Null)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(before, after);
    }
    assert_eq!(
        admin(
            &g,
            "PUT",
            "/admin/users/a",
            json!({"name":"updated","max_concurrent":3})
        )
        .await
        .status(),
        200
    );
    let updated: Value = admin(&g, "GET", "/admin/users/a", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["user"]["keys"], before["user"]["keys"]);
    assert_eq!(updated["user"]["max_concurrent"], 3);
    assert_eq!(
        admin(&g, "DELETE", "/admin/users/a", Value::Null)
            .await
            .status(),
        200
    );
    assert_eq!(generate(&g, "a-secret", false).await.status(), 401);
    let checkpoint: Value = admin(&g, "GET", "/admin/checkpoint", Value::Null)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        checkpoint["state"]["bundle"]["secrets"]
            .get("user-key-a-key")
            .is_none()
    );
    assert!(
        admin(&g, "POST", "/admin/apply", json!({"config":{"users":[]}}))
            .await
            .status()
            .is_client_error()
    );
}

#[tokio::test]
async fn all_keys_share_user_concurrency_across_rotation_delete_recreate_and_stream_cancel() {
    let reply = Reply { tail: Some((Duration::from_secs(30),"data: [DONE]\n\n".into())),
        ..Reply::sse("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"one\"}}\n\n".into()) };
    let mock = mock(vec![
        reply,
        Reply::json(native(Provider::OpenAI)),
        Reply::json(native(Provider::OpenAI)),
    ])
    .await;
    let g = gateway(configuration(&mock)).await;
    let mut a = user("a", "a-secret");
    a["keys"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"a-other","key":"a-other-secret"}));
    assert_eq!(
        admin(&g, "POST", "/admin/users", a.clone()).await.status(),
        200
    );
    assert_eq!(
        admin(&g, "POST", "/admin/users", user("b", "b-secret"))
            .await
            .status(),
        200
    );
    let stream = generate(&g, "a-secret", true).await;
    assert_eq!(stream.status(), 200);
    assert_eq!(generate(&g, "b-secret", false).await.status(), 200);
    let blocked = generate(&g, "a-other-secret", false).await;
    assert_eq!(blocked.status(), 429);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "user_concurrency_limit"
    );
    a["keys"][0]["key"] = json!("rotated");
    assert_eq!(
        admin(&g, "PUT", "/admin/users/a", a.clone()).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-secret", false).await.status(), 401);
    assert_eq!(generate(&g, "rotated", false).await.status(), 429);
    // Raising and then lowering the live limit does not reset the active count.
    let mut raised = a.clone();
    raised["max_concurrent"] = json!(2);
    assert_eq!(
        admin(&g, "PUT", "/admin/users/a", raised).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 200);
    assert_eq!(
        admin(&g, "PUT", "/admin/users/a", a.clone()).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 429);
    assert_eq!(
        admin(&g, "DELETE", "/admin/users/a", Value::Null)
            .await
            .status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 401);
    assert_eq!(admin(&g, "POST", "/admin/users", a).await.status(), 200);
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 429);
    drop(stream);
    for _ in 0..100 {
        let u: Value = admin(&g, "GET", "/admin/users/a", Value::Null)
            .await
            .json()
            .await
            .unwrap();
        if u["user"]["in_flight"] == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled stream leaked a user permit");
}
