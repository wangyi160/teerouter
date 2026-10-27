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
    assert!(accounts.as_object().unwrap().is_empty());
    assert!(secrets.as_object().unwrap().iter().all(|(k,v)| k=="upstream" && (v=="MERCHANT_PRIVATE_UPSTREAM_CREDENTIAL" || v=="REJECTED_PRIVATE")));
    let (status,result)=call(s,Method::PUT,&format!("/admin/merchants/{mid}/config"),ADMIN,
        json!({"runtime_id":runtime_id,"operation_id":crate::vault::random_id().unwrap(),"config":body})).await;
    (status, result.get("state").cloned().unwrap_or(result))
}

#[tokio::test]
async fn public_commands_are_idempotent_and_credential_tickets_are_rejected() {
    let r=root();let token=control("a");add(&r,&token).await;
    let s=serve(crate::server::app_runtime(r.clone())).await;
    let snapshot=state(&r,&registration(&token)).unwrap();
    let path=format!("/admin/merchants/{}/config",crypto::identity(&token));
    let mut config=update(2);config.as_object_mut().unwrap().remove("secrets");
    let command=json!({"runtime_id":snapshot["merchant"]["runtime_id"],"operation_id":"public-edit","config":config});
    let (code,result)=call(&s,Method::PUT,&path,ADMIN,command.clone()).await;
    assert_eq!(code,200,"{result}");
    assert_eq!(call(&s,Method::PUT,&path,ADMIN,command.clone()).await.1["applied_revision"],3);
    let mut old=command;old["ticket"]=json!("a".repeat(64));
    assert_eq!(call(&s,Method::PUT,&path,ADMIN,old).await.0,400);
    assert_eq!(call(&s,Method::PUT,"/merchant/accounts/prepare",&token,json!({})).await.0,404);
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
    assert_eq!(status, 404);
}
