use super::*;

async fn routed(server: &Server, token: &str, pool: Option<&str>) -> (u16, Value) {
    let mut policy = json!({"version":1,"mode":if pool.is_some(){"fixed"}else{"auto"},"priority":"price","max_multiplier_millionths":1000000});
    if let Some(pool) = pool {
        policy["pool"] = pool.into();
    }
    let response = reqwest::Client::builder()
        .no_proxy()
        .user_agent("codex_cli_rs/1.0")
        .build()
        .unwrap()
        .post(format!("{}/v1/chat/completions", server.url))
        .bearer_auth(token)
        .header("x-router-routing", policy.to_string())
        .json(&json!({"model":"chatgpt/model-test","messages":[{"role":"user","content":"test"}]}))
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

#[tokio::test]
async fn refusal_limit_is_per_user_across_keys_and_scoped_to_one_merchant() {
    let r = root();
    let mut config = r.engine().config.clone();
    let mut second_key = config.keys[0].clone();
    second_key.name = "alice-second".into();
    second_key.key_env = "ALICE_SECOND".into();
    config.keys.push(second_key);
    r.apply(
        Update {
            config: Some(serde_json::to_value(config).unwrap()),
            secrets: BTreeMap::from([("ALICE_SECOND".into(), "alice-second-key".into())]),
            ..Default::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    let a = control("a");
    let b = control("b");
    let first = add(&r, &a).await;
    let second = add(&r, &b).await;
    let mut registered = registration(&a);
    registered.users.insert("bob".into());
    register(&r, registered, ADMIN).await.unwrap();
    let mut limits = update(2);
    limits["customer_defaults"] = json!({"refusal_limit":1});
    apply(&r, &a, serde_json::from_value(limits).unwrap())
        .await
        .unwrap();
    let _one = fixture(&first, completion()).await;
    let _two = fixture(&second, completion()).await;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let pa = format!("{}.official", crypto::identity(&a));
    let pb = format!("{}.official", crypto::identity(&b));
    let first_reply = routed(&server, "alice-key", Some(&pa)).await;
    assert_eq!(first_reply.0, 200, "{:?}", first_reply.1);
    assert_eq!(routed(&server, "alice-second-key", Some(&pa)).await.0, 200); // equal to threshold was allowed
    for key in ["alice-key", "alice-second-key"] {
        let (status, error) = routed(&server, key, Some(&pa)).await;
        assert_eq!(status, 403, "{error:?}");
        assert_eq!(error["error"]["code"], "merchant_refusal_limit_exceeded");
    }
    let state = state(&r, &registration(&a)).unwrap();
    assert_eq!(state["customer_refusals"]["alice"]["count"], 2);
    assert_eq!(state["customer_refusals"]["alice"]["blocked"], true);
    assert_eq!(routed(&server, "bob-key", Some(&pa)).await.0, 200);
    assert_eq!(routed(&server, "alice-key", Some(&pb)).await.0, 200);
    assert_eq!(routed(&server, "alice-key", None).await.0, 200); // another merchant remains usable
    // Replacing an API key for the same user cannot evade the ban.
    r.apply(
        Update {
            secrets: BTreeMap::from([("ALICE_SECOND".into(), "alice-regenerated".into())]),
            ..Default::default()
        },
        ADMIN,
    )
    .await
    .unwrap();
    assert_eq!(call(&server,Method::POST,"/v1/chat/completions","alice-regenerated",
        json!({"model":format!("{}/model-test",crypto::identity(&a)),"messages":[{"role":"user","content":"test"}]})).await.0,403);
    let (_, models) = call(&server, Method::GET, "/v1/models", "alice-key", Value::Null).await;
    assert!(!models.to_string().contains(&pa));
    assert!(models.to_string().contains(&pb));
    // Reset only the enforcement baseline, preserving the lifetime total.
    let mut clear = update(3);
    let mut customer = state["settings"]["customers"][0].clone();
    customer["refusal_baseline"] = 2.into();
    clear["customers"] = json!([customer]);
    let result = apply(&r, &a, serde_json::from_value(clear).unwrap())
        .await
        .unwrap();
    assert_eq!(result["customer_refusals"]["alice"]["count"], 0);
    assert_eq!(result["customer_refusals"]["alice"]["total"], 2);
    assert_eq!(result["customer_refusals"]["alice"]["blocked"], false);
    let mut stale = update(4);
    stale["customers"] = state["settings"]["customers"].clone();
    let result = apply(&r, &a, serde_json::from_value(stale).unwrap())
        .await
        .unwrap();
    assert_eq!(result["customer_refusals"]["alice"]["count"], 0); // stale form cannot undo reset
    let _again = fixture(&first, completion()).await;
    assert_eq!(routed(&server, "alice-regenerated", Some(&pa)).await.0, 200);
    // History proves previous use, so restart cannot hide a blocked customer.
    r.merchants
        .live
        .lock()
        .unwrap()
        .remove(&crypto::identity(&a));
    let restored = unlock(&r, &a, 0).await.unwrap();
    assert_eq!(restored["customer_refusals"]["alice"]["total"], 3);
    assert!(
        restored["settings"]["customers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["user_id"] == "alice")
    );
}

#[tokio::test]
async fn single_merchant_auto_rule_returns_refusal_error_and_zero_disables_limit() {
    let r = root();
    let a = control("a");
    let live = add(&r, &a).await;
    let _upstream = fixture(&live, completion()).await;
    r.engine()
        .vault
        .lock()
        .unwrap()
        .merchant_refusal_counts
        .insert(crypto::identity(&a), BTreeMap::from([("alice".into(), 4)]));
    live.settings
        .lock()
        .unwrap()
        .customer_defaults
        .refusal_limit = 1;
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let (status, error) = routed(&server, "alice-key", None).await;
    assert_eq!(status, 403, "{error:?}");
    assert_eq!(error["error"]["code"], "merchant_refusal_limit_exceeded");
    live.settings.lock().unwrap().customers.push(
        serde_json::from_value(json!({
            "user_id":"alice","enabled":true,"models":[],"max_concurrent":16,"refusal_limit":0
        }))
        .unwrap(),
    );
    assert_eq!(routed(&server, "alice-key", None).await.0, 200);
    live.settings.lock().unwrap().customers.clear();
    live.settings
        .lock()
        .unwrap()
        .customer_defaults
        .refusal_limit = 0;
    assert_eq!(routed(&server, "alice-key", None).await.0, 200);
}
