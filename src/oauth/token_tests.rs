use super::*;
use rstest::rstest;

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[tokio::test]
async fn externally_managed_tokens_expire_and_recover_without_network(#[case] provider: Provider) {
    let vault = crate::vault::Vault::memory().unwrap();
    // An unreachable proxy proves that token resolution cannot perform renewal.
    vault
        .import_account(
            "test",
            provider,
            &json!({"access_token":"current","expires_at":now()+30.0}),
        )
        .unwrap();
    vault
        .set_account_proxy("test", provider, "http://127.0.0.1:1")
        .unwrap();
    let store = TokenStore::open(provider, vault.clone(), "test").unwrap();
    assert_eq!(store.credential().await.unwrap().access_token, "current");
    vault
        .import_account(
            "test",
            provider,
            &json!({"access_token":"expired","expires_at":1}),
        )
        .unwrap();
    assert!(store.credential().await.is_err());
    assert!(!store.available());
    vault
        .import_account(
            "test",
            provider,
            &json!({"access_token":"renewed","expires_at":now()+3600.0}),
        )
        .unwrap();
    assert!(store.available());
    assert_eq!(store.credential().await.unwrap().access_token, "renewed");
    store.rejected();
    assert!(!store.available());
    assert!(store.credential().await.is_err());
    vault
        .import_account("test", provider, &json!({"access_token":"replacement"}))
        .unwrap();
    assert_eq!(
        store.credential().await.unwrap().access_token,
        "replacement"
    );
    vault
        .set_account_proxy("test", provider, "http://127.0.0.1:2")
        .unwrap();
    assert!(store.credential().await.is_err());
}

#[rstest]
#[case(Provider::OpenAI)]
#[case(Provider::Anthropic)]
#[case(Provider::XAI)]
#[test]
fn every_storage_entry_rejects_refresh_credentials(#[case] provider: Provider) {
    let vault = crate::vault::Vault::memory().unwrap();
    for bad in [
        json!({"access_token":"valid","refresh_token":"FORBIDDEN"}),
        json!({"access_token":"valid","refresh_token":null}),
        json!({"access_token":"valid","refreshToken":"FORBIDDEN"}),
        json!({"access_token":"valid","extra":{"refresh_token":"FORBIDDEN"}}),
        json!({"claudeAiOauth":{"accessToken":"valid","refreshToken":"FORBIDDEN"}}),
        json!({"refresh_token":"FORBIDDEN"}),
    ] {
        assert!(vault.import_account("a", provider, &bad).is_err());
        assert!(vault.put_account("a", provider, &bad).is_err());
        assert!(vault.account("a", provider).is_err());
        let provision = crate::vault::Provision {
            config: serde_json::from_value(
                json!({"server":{"allow_empty_routes":true},"keys":[],"models":[]}),
            )
            .unwrap(),
            secrets: Default::default(),
            accounts: std::collections::BTreeMap::from([(
                "a".into(),
                serde_json::from_value(
                    json!({"provider":provider,"credential":bad,"enabled":false}),
                )
                .unwrap(),
            )]),
        };
        assert!(
            vault.provision(&provision).is_err(),
            "disabled accounts must reject refresh tokens too"
        );
    }
}

#[tokio::test]
async fn claude_access_only_legacy_wrapper_and_jwt_expiry_work() {
    let vault = crate::vault::Vault::memory().unwrap();
    vault
        .import_account(
            "a",
            Provider::Anthropic,
            &json!({"claudeAiOauth":{"accessToken":"claude","expiresAt":(now()+3600.0)*1000.0}}),
        )
        .unwrap();
    vault
        .set_account_proxy("a", Provider::Anthropic, "http://127.0.0.1:1")
        .unwrap();
    assert_eq!(
        TokenStore::open(Provider::Anthropic, vault, "a")
            .unwrap()
            .credential()
            .await
            .unwrap()
            .access_token,
        "claude"
    );
    let jwt = format!(
        "e30.{}.fixture",
        URL_SAFE_NO_PAD.encode(json!({"exp":123}).to_string())
    );
    assert_eq!(access_expiry(&json!({"access_token":jwt})), Some(123.0));
}
