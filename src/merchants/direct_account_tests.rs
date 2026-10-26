use super::*;

fn input(r: &Runtime, token: &str) -> Value {
    let value = state(r, &registration(token)).unwrap();
    json!({"runtime_id":value["merchant"]["runtime_id"],"operation_id":"owner-sync",
        "expected_revision":value["settings"]["revision"],"pools":value["pools"],
        "secrets":{"upstream":"NEW_OWNER_PRIVATE_TOKEN"}})
}

#[tokio::test]
async fn owner_accounts_activate_without_platform_and_retries_are_idempotent() {
    let r=root();let token=control("a");let live=add(&r,&token).await;
    let s=serve(crate::server::app_runtime(r.clone())).await;
    let body=input(&r,&token);
    assert_eq!(call(&s,Method::PUT,"/merchant/accounts/sync",ADMIN,body.clone()).await.0,403);
    let (status,ack)=call(&s,Method::PUT,"/merchant/accounts/sync",&token,body.clone()).await;
    assert_eq!(status,200,"{ack}");assert_eq!(ack["settings"]["revision"],3);
    assert_eq!(live.runtime.engine().vault.snapshot().unwrap().bundle.secrets["upstream"],"NEW_OWNER_PRIVATE_TOKEN");
    assert!(!ack.to_string().contains("PRIVATE"));
    assert_eq!(call(&s,Method::PUT,"/merchant/accounts/sync",&token,body.clone()).await.1["settings"]["revision"],3);
    let mut conflict=body.clone();conflict["secrets"]["upstream"]=json!("different");
    assert_eq!(call(&s,Method::PUT,"/merchant/accounts/sync",&token,conflict).await.0,409);
    let (_,snapshot)=call(&s,Method::GET,&format!("/admin/merchants/{}/view",crypto::identity(&token)),ADMIN,Value::Null).await;
    assert_eq!(snapshot["settings"]["revision"],3);assert!(!snapshot.to_string().contains("PRIVATE"));
}

#[tokio::test]
async fn account_channel_preserves_existing_policies_and_rejects_refresh_tokens() {
    let r=root();let token=control("a");add(&r,&token).await;
    let before=state(&r,&registration(&token)).unwrap();let s=serve(crate::server::app_runtime(r.clone())).await;
    let mut body=input(&r,&token);
    body["pools"][0]["allowed_models"]=json!(["unauthorized-policy-edit"]);
    let (status,ack)=call(&s,Method::PUT,"/merchant/accounts/sync",&token,body).await;
    assert_eq!(status,200,"{ack}");assert_eq!(ack["pools"][0]["allowed_models"],before["pools"][0]["allowed_models"]);
    assert_eq!(ack["settings"]["pool_pricing"],before["settings"]["pool_pricing"]);
    let mut body=input(&r,&token);body["operation_id"]=json!("bad-refresh");
    body["accounts"]=json!({"a":{"provider":"xai","credential":{"access_token":"ACCESS","refresh_token":"FORBIDDEN_REFRESH"}}});
    assert_ne!(call(&s,Method::PUT,"/merchant/accounts/sync",&token,body).await.0,200);
    assert_eq!(state(&r,&registration(&token)).unwrap()["settings"]["revision"],3);
    let mut body=input(&r,&token);body["operation_id"]=json!("wrong-runtime");body["runtime_id"]=json!("wrong");
    assert_eq!(call(&s,Method::PUT,"/merchant/accounts/sync",&token,body).await.0,409);
}
