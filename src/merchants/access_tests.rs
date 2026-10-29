use super::*;
use crate::Provider;

#[tokio::test]
async fn owner_access_sync_is_scoped_retry_safe_and_rejects_refresh_tokens() {
    let r = root();
    let owner = control("a");
    add(&r, &owner).await;
    let other = control("b");
    add(&r, &other).await;
    let mut config = update(2);
    config["pools"][0]["deployments"] =
        json!([{"id":"account","provider":"openai","oauth_account":"a","enabled":false}]);
    config["accounts"] = json!({"a":{"provider":"openai","credential":{"access_token":"OLD","account_id":"workspace"},"proxy":"http://127.0.0.1:1","enabled":false}});
    let initial = apply(&r, &owner, serde_json::from_value(config.clone()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        initial["merchant"]["credential_protocol"],
        "access-token-only-v1"
    );
    let runtime_id = initial["merchant"]["runtime_id"].clone();
    let old = crate::oauth::tokens::access_fingerprint(&json!({"access_token":"OLD"}));
    let update = json!({"runtime_id":runtime_id,"accounts":{"a":{"expected_access_sha256":old,"credential":{"access_token":"NEW","account_id":"workspace"}}}});
    let server = serve(crate::server::app_runtime(r.clone())).await;
    for token in [ADMIN, "alice-key", &"a".repeat(64)] {
        assert_eq!(
            call(
                &server,
                Method::PUT,
                "/merchant/accounts/access",
                token,
                update.clone()
            )
            .await
            .0,
            403
        );
    }
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/merchant/accounts/access",
            &other,
            update.clone()
        )
        .await
        .0,
        409
    );
    let (code, first) = call(
        &server,
        Method::PUT,
        "/merchant/accounts/access",
        &owner,
        update.clone(),
    )
    .await;
    assert_eq!(code, 200);
    let (code, retry) = call(
        &server,
        Method::PUT,
        "/merchant/accounts/access",
        &owner,
        update.clone(),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        first["settings"]["revision"],
        initial["settings"]["revision"]
    );
    assert_eq!(
        first["accounts"]["data"][0]["revision"],
        retry["accounts"]["data"][0]["revision"]
    );
    for key in ["refresh_token", "refreshToken", "extra"] {
        let mut bad = update.clone();
        bad["accounts"]["a"]["credential"][key] = json!("NEVER_HOLD");
        assert_eq!(
            call(
                &server,
                Method::PUT,
                "/merchant/accounts/access",
                &owner,
                bad
            )
            .await
            .0,
            400
        );
    }
    let mut stale = update.clone();
    stale["accounts"]["a"]["credential"]["access_token"] = "STALE".into();
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/merchant/accounts/access",
            &owner,
            stale
        )
        .await
        .0,
        409
    );
    let mut reboot = update.clone();
    reboot["runtime_id"] = "older-runtime".into();
    assert_eq!(
        call(
            &server,
            Method::PUT,
            "/merchant/accounts/access",
            &owner,
            reboot
        )
        .await
        .0,
        409
    );
    // General config writes must enforce the same restriction, including disabled accounts.
    config["expected_revision"] = initial["settings"]["revision"].clone();
    config["accounts"]["a"]["credential"]["refresh_token"] = "NEVER_HOLD".into();
    assert!(
        apply(&r, &owner, serde_json::from_value(config).unwrap())
            .await
            .is_err()
    );
    let vault = live(&r, &crypto::identity(&owner))
        .unwrap()
        .runtime
        .engine()
        .vault
        .clone();
    assert_eq!(
        vault.account("a", Provider::OpenAI).unwrap().0["access_token"],
        "NEW"
    );
    assert!(
        !vault
            .checkpoint_export(None, None)
            .unwrap()
            .to_string()
            .contains("NEVER_HOLD")
    );
    let (code, _) = call(
        &server,
        Method::GET,
        "/merchant/accounts/credentials",
        &owner,
        Value::Null,
    )
    .await;
    assert_eq!(code, 404);
}
