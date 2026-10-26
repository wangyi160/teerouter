use super::*;

pub(super) async fn submit(s: &Server, r: &Runtime, token: &str, mut body: Value) -> (u16, Value) {
    let mid = crypto::identity(token);
    let snapshot = state(r, &registration(token)).unwrap();
    let runtime_id = snapshot["merchant"]["runtime_id"].clone();
    let accounts = body
        .as_object_mut()
        .unwrap()
        .remove("accounts")
        .unwrap_or(json!({}));
    let secrets = body
        .as_object_mut()
        .unwrap()
        .remove("secrets")
        .unwrap_or(json!({}));
    let public_digest = crypto::digest(&serde_json::to_string(&body).unwrap());
    let (status,prepared)=call(s,Method::PUT,"/merchant/accounts/prepare",token,
        json!({"runtime_id":runtime_id,"expected_revision":body["expected_revision"],"public_digest":public_digest,
            "material":{"accounts":accounts,"secrets":secrets}})).await;
    if status != 200 {
        return (status, prepared);
    }
    let (status,result)=call(s,Method::PUT,&format!("/admin/merchants/{mid}/config"),ADMIN,
        json!({"runtime_id":runtime_id,"operation_id":crate::vault::random_id().unwrap(),"config":body,"ticket":prepared["ticket"]})).await;
    (status, result.get("state").cloned().unwrap_or(result))
}

#[tokio::test]
async fn public_commands_commit_prepared_credentials_atomically_and_are_retry_safe() {
    let r = root();
    let token = control("a");
    let live = add(&r, &token).await;
    let other = control("b");
    add(&r, &other).await;
    let locked = control("c");
    register(&r, registration(&locked), ADMIN).await.unwrap();
    let s = serve(crate::server::app_runtime(r.clone())).await;
    let state = state(&r, &registration(&token)).unwrap();
    let runtime_id = state["merchant"]["runtime_id"].clone();
    let mut config = update(2);
    config.as_object_mut().unwrap().remove("secrets");
    let prepare = |config: &Value| {
        json!({"runtime_id":runtime_id,"expected_revision":2,
        "public_digest":crypto::digest(&serde_json::to_string(config).unwrap()),
        "material":{"secrets":{"upstream":"NEW_PRIVATE_TOKEN"}}})
    };
    let (_, staged) = call(
        &s,
        Method::PUT,
        "/merchant/accounts/prepare",
        &token,
        prepare(&config),
    )
    .await;
    assert_eq!(
        live.runtime
            .engine()
            .vault
            .snapshot()
            .unwrap()
            .bundle
            .secrets["upstream"],
        "MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL"
    );
    let command = json!({"runtime_id":runtime_id,"operation_id":"atomic","ticket":staged["ticket"],"config":config});
    let path = format!("/admin/merchants/{}/config", crypto::identity(&token));
    let mut foreign = command.clone();
    foreign["runtime_id"] =
        super::super::state(&r, &registration(&other)).unwrap()["merchant"]["runtime_id"].clone();
    assert_eq!(
        call(
            &s,
            Method::PUT,
            &format!("/admin/merchants/{}/config", crypto::identity(&other)),
            ADMIN,
            foreign
        )
        .await
        .0,
        409
    );
    assert_eq!(
        call(&s, Method::PUT, &path, &token, command.clone())
            .await
            .0,
        401
    );
    let mut wrong = command.clone();
    wrong["config"]["max_concurrent"] = json!(8);
    assert_eq!(call(&s, Method::PUT, &path, ADMIN, wrong).await.0, 409);
    assert_eq!(live.settings.lock().unwrap().revision, 2);
    let (status, ack) = call(&s, Method::PUT, &path, ADMIN, command.clone()).await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["applied_revision"], 3);
    assert!(!ack.to_string().contains("PRIVATE"));
    assert_eq!(
        live.runtime
            .engine()
            .vault
            .snapshot()
            .unwrap()
            .bundle
            .secrets["upstream"],
        "NEW_PRIVATE_TOKEN"
    );
    assert_eq!(
        call(&s, Method::PUT, &path, ADMIN, command.clone()).await.1["applied_revision"],
        3
    );
    assert_eq!(live.settings.lock().unwrap().revision, 3);
    let mut wrong = command.clone();
    wrong["ticket"] = json!("f".repeat(64));
    assert_eq!(call(&s, Method::PUT, &path, ADMIN, wrong).await.0, 409);
    let (_, views) = call(&s, Method::GET, "/admin/merchant-views", ADMIN, Value::Null).await;
    assert_eq!(views["data"].as_array().unwrap().len(), 2);
    assert_eq!(views["locked"], json!([crypto::identity(&locked)]));
    assert!(!views.to_string().contains("PRIVATE"));
    assert_eq!(
        call(&s, Method::GET, "/merchant/state", &token, Value::Null)
            .await
            .0,
        404
    );
    assert_eq!(
        call(&s, Method::PUT, "/merchant/config", &token, update(3))
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn staged_credential_cannot_overwrite_a_newer_access_upload() {
    let r = root();
    let owner = control("a");
    let live = add(&r, &owner).await;
    let mut config = update(2);
    config["pools"][0]["deployments"] =
        json!([{"id":"account","provider":"openai","oauth_account":"a","enabled":false}]);
    config["accounts"] = json!({"a":{"provider":"openai","credential":{"access_token":"OLD","account_id":"workspace"},"proxy":"http://127.0.0.1:1","enabled":false}});
    let initial = apply(&r, &owner, serde_json::from_value(config.clone()).unwrap())
        .await
        .unwrap();
    config["expected_revision"] = json!(3);
    config.as_object_mut().unwrap().remove("accounts");
    config.as_object_mut().unwrap().remove("secrets");
    let runtime_id = initial["merchant"]["runtime_id"].clone();
    let s = serve(crate::server::app_runtime(r.clone())).await;
    let (status,prepared)=call(&s,Method::PUT,"/merchant/accounts/prepare",&owner,json!({"runtime_id":runtime_id,
        "expected_revision":3,"public_digest":crypto::digest(&serde_json::to_string(&config).unwrap()),
        "material":{"accounts":{"a":{"credential":{"access_token":"STAGED","account_id":"workspace"}}}}})).await;
    assert_eq!(status, 200, "{prepared}");
    let (status,_)=call(&s,Method::PUT,"/merchant/accounts/access",&owner,json!({"runtime_id":runtime_id,"accounts":{"a":{
        "expected_access_sha256":crate::oauth::tokens::access_fingerprint(&json!({"access_token":"OLD"})),
        "credential":{"access_token":"NEWER","account_id":"workspace"}}}})).await;
    assert_eq!(status, 200);
    let (status,error)=call(&s,Method::PUT,&format!("/admin/merchants/{}/config",crypto::identity(&owner)),ADMIN,
        json!({"operation_id":"staged-race","runtime_id":runtime_id,"config":config,"ticket":prepared["ticket"]})).await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "account_credential_changed");
    assert_eq!(
        live.runtime
            .engine()
            .vault
            .snapshot()
            .unwrap()
            .bundle
            .accounts["a"]
            .credential["access_token"],
        "NEWER"
    );
    assert_eq!(live.settings.lock().unwrap().revision, 3);
}

#[tokio::test]
async fn failed_validation_never_activates_secrets_and_platform_cannot_supply_private_fields() {
    let r = root();
    let token = control("a");
    let live = add(&r, &token).await;
    let s = serve(crate::server::app_runtime(r.clone())).await;
    let mut invalid = update(2);
    invalid["pool_pricing"][0]["multiplier_millionths"] = json!(1_000_000_000_000_002u64);
    invalid["secrets"]["upstream"] = json!("REJECTED_PRIVATE");
    assert_eq!(submit(&s, &r, &token, invalid).await.0, 400);
    assert_eq!(live.settings.lock().unwrap().revision, 2);
    assert_eq!(
        live.runtime
            .engine()
            .vault
            .snapshot()
            .unwrap()
            .bundle
            .secrets["upstream"],
        "MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL"
    );
    let runtime_id = state(&r, &registration(&token)).unwrap()["merchant"]["runtime_id"].clone();
    let path = format!("/admin/merchants/{}/config", crypto::identity(&token));
    for mut public in [update(2), update(2)] {
        public["accounts"] = json!({});
        let reply = call(
            &s,
            Method::PUT,
            &path,
            ADMIN,
            json!({"runtime_id":runtime_id,"operation_id":"reject","config":public}),
        )
        .await;
        assert_eq!(reply.0, 400);
    }
    let (status,_)=call(&s,Method::PUT,"/merchant/accounts/prepare",&token,json!({"runtime_id":runtime_id,
        "expected_revision":2,"public_digest":"a".repeat(64),"material":{"accounts":{"new":{"provider":"xai",
        "credential":{"access_token":"access","refresh_token":"PRIVATE_REFRESH"}}}}})).await;
    assert_eq!(status, 400);
}
