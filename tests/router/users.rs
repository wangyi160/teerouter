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
async fn managed_balance_blocks_upstream_and_accepts_topup() {
    let mock=mock(vec![Reply::json(native(Provider::OpenAI))]).await;
    let engine=engine(configuration(&mock));engine.vault.require_manager();
    let initial=engine.vault.lock().unwrap().clone();engine.vault.initialize(initial).unwrap();
    let g=run_fixture(engine.clone()).await;
    for (balance,status) in [(None,503),(Some(0),402),(Some(-1),402),(Some(100),200)] {
        let mut data=engine.vault.lock().unwrap();
        if let Some(balance)=balance {data.balances.insert("test-user".into(),balance);} else {data.balances.clear();}
        drop(data);
        assert_eq!(generate(&g,"client-secret",false).await.status(),status);
    }
    assert_eq!(mock.calls.lock().unwrap().len(),1);
}
fn effective(g:&Running)->usize {
    let e=g.runtime.as_ref().unwrap().engine();let data=e.vault.lock().unwrap();
    crate::balances::concurrency_limit(&data,&e.config.users[0],&e.config.balance_concurrency).unwrap()
}
async fn replace_user(g:&Running,id:&str,body:Option<Value>)->reqwest::Response {
    let e=g.runtime.as_ref().unwrap().engine();let mut config=e.config.clone();
    config.users.retain(|u|u.id!=id);config.keys.retain(|k|k.user_id!=id);
    let mut secrets=serde_json::Map::new();
    if let Some(mut body)=body {
        let keys=body.as_object_mut().unwrap().remove("keys").unwrap();
        config.users.push(serde_json::from_value(body).unwrap());
        for key in keys.as_array().unwrap() {
            let name=key["name"].as_str().unwrap();let reference=format!("fixture-{name}");
            config.keys.push(serde_json::from_value(json!({"name":name,"user_id":id,"key_ref":reference})).unwrap());
            secrets.insert(reference,key["key"].clone());
        }
    }
    admin(g,"POST","/admin/apply",json!({"config":config,"secrets":secrets})).await
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
    assert_eq!(effective(&g), 2);
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
    assert_eq!(effective(&g), 1);
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
        replace_user(&g,"a",Some(a.clone())).await.status(),
        200
    );
    assert_eq!(
        replace_user(&g,"b",Some(user("b","b-secret")))
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
        replace_user(&g,"a",Some(a.clone())).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-secret", false).await.status(), 401);
    assert_eq!(generate(&g, "rotated", false).await.status(), 429);
    // Raising and then lowering the live limit does not reset the active count.
    let mut raised = a.clone();
    raised["max_concurrent"] = json!(2);
    assert_eq!(
        replace_user(&g,"a",Some(raised)).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 200);
    assert_eq!(
        replace_user(&g,"a",Some(a.clone())).await.status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 429);
    assert_eq!(
        replace_user(&g,"a",None)
            .await
            .status(),
        200
    );
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 401);
    assert_eq!(replace_user(&g,"a",Some(a)).await.status(), 200);
    assert_eq!(generate(&g, "a-other-secret", false).await.status(), 429);
    drop(stream);
    for _ in 0..100 {
        if g.runtime.as_ref().unwrap().engine().status()["in_flight"] == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cancelled stream leaked a user permit");
}
