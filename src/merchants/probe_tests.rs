use super::*;

#[tokio::test]
async fn unpublished_pool_probe_is_disposable_and_cannot_override_published_policy() {
    let r = root();
    let token = control("a");
    add(&r, &token).await;
    let before = state(&r, &registration(&token)).unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let body = json!({"pool":"local-draft","draft_pool":{"provider":"openai","allowed_models":["model-test"]},
        "proxy":"http://127.0.0.1:1","credential":{"access_token":"PRIVATE_EXPIRED","expires_at":1}});
    let path = "/merchant/accounts/test";
    let (status, result) = call(&server, Method::POST, path, &token, body.clone()).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["code"], "credential_invalid_or_expired");
    assert_eq!(state(&r, &registration(&token)).unwrap(), before);
    let mut existing = body.clone();
    existing["pool"] = "official".into();
    assert_eq!(
        call(&server, Method::POST, path, &token, existing).await.0,
        400
    );
    for invalid in [
        json!(["*"]),
        json!(["x", "x"]),
        json!(["a/../b"]),
        json!(vec!["x"; 4097]),
    ] {
        let mut draft = body.clone();
        draft["draft_pool"]["allowed_models"] = invalid;
        assert_eq!(
            call(&server, Method::POST, path, &token, draft).await.0,
            400
        );
    }
    let mut third_party = body.clone();
    third_party["draft_pool"]["base_url"] = "https://third-party.example".into();
    assert_eq!(
        call(&server, Method::POST, path, &token, third_party)
            .await
            .0,
        400
    );
    let mut test_provider = body;
    test_provider["draft_pool"]["provider"] = "workbuddy".into();
    assert_eq!(
        call(&server, Method::POST, path, &token, test_provider)
            .await
            .0,
        403
    );
    assert_eq!(state(&r, &registration(&token)).unwrap(), before);
}

#[tokio::test]
async fn account_probe_is_authenticated_scoped_bounded_and_does_not_save_or_refresh() {
    let r = root();
    let token = control("a");
    let live = add(&r, &token).await;
    let before = state(&r, &registration(&token)).unwrap();
    let server = serve(crate::server::app_runtime(r.clone())).await;
    let path = "/merchant/accounts/test";
    let body = json!({"pool":"official","proxy":"http://127.0.0.1:1",
        "credential":{"access_token":"PRIVATE_EXPIRED","expires_at":1,"refresh_token":"PRIVATE_REFRESH"}});
    for key in [ADMIN, "alice-key", "unknown"] {
        assert_eq!(
            call(&server, Method::POST, path, key, body.clone()).await.0,
            403
        );
    }
    let mut other = body.clone();
    other["pool"] = "another-merchant-pool".into();
    assert_eq!(
        call(&server, Method::POST, path, &token, other).await.0,
        404
    );
    for proxy in ["", "direct://"] {
        let mut invalid = body.clone();
        invalid["proxy"] = proxy.into();
        assert_eq!(
            call(&server, Method::POST, path, &token, invalid).await.1["error"]["code"],
            "probe_proxy_required"
        );
    }
    let mut override_host = body.clone();
    override_host["base_url"] = "https://third-party.example".into();
    assert_eq!(
        call(&server, Method::POST, path, &token, override_host)
            .await
            .0,
        400
    );
    let permit = live.probe_gate.acquire(1).unwrap();
    assert_eq!(
        call(&server, Method::POST, path, &token, body.clone())
            .await
            .0,
        429
    );
    drop(permit);
    let (status, result) = call(&server, Method::POST, path, &token, body).await;
    assert_eq!(status, 200);
    // An expired access token fails locally; the supplied refresh token is never used.
    assert_eq!(result["code"], "credential_invalid_or_expired");
    assert!(!result.to_string().contains("PRIVATE_"));
    assert_eq!(state(&r, &registration(&token)).unwrap(), before);
    assert_eq!(live.probe_gate.used(), 0);
    assert_eq!(r.merchants.probe_gate.used(), 0);
}
