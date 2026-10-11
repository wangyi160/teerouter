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
