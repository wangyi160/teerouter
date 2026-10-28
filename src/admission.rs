//! Login-issued, narrowly scoped credentials for the pre-attestation channel.
//! These tokens never authorize business endpoints or carry upstream secrets.
use crate::{error::ApiError, vault::Vault};
use axum::http::HeaderMap;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::hmac;
use serde::Deserialize;

pub const HEADER: &str = "x-router-access";
const AUDIENCE: &str = "teerouter-handshake-v1";
const LIFETIME: u64 = 30 * 86400;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    role: String,
    scope: String,
    iat: u64,
    exp: u64,
}

pub(crate) fn authorize(vault: &Vault, headers: &HeaderMap) -> Result<(), ApiError> {
    let denied = || {
        ApiError::new(
            401,
            "router_access_required",
            "Valid login-issued Router access credential required",
        )
    };
    if headers.get_all(HEADER).iter().count() != 1 {
        return Err(denied());
    }
    let token = headers
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= 2048)
        .ok_or_else(denied)?;
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(denied());
    }
    let header = B64.decode(parts[0]).map_err(|_| denied())?;
    let header: serde_json::Value = serde_json::from_slice(&header).map_err(|_| denied())?;
    if header != serde_json::json!({"alg":"HS256","typ":"JWT"}) {
        return Err(denied());
    }
    let signature = B64.decode(parts[2]).map_err(|_| denied())?;
    let data = vault.lock().map_err(|_| denied())?;
    let bundle = data.bundle.as_ref().ok_or_else(denied)?;
    let key = bundle
        .config
        .server
        .router_access_key_ref
        .as_ref()
        .and_then(|name| bundle.secrets.get(name))
        .ok_or_else(denied)?;
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()),
        format!("{}.{}", parts[0], parts[1]).as_bytes(),
        &signature,
    )
    .map_err(|_| denied())?;
    let claims: Claims = serde_json::from_slice(&B64.decode(parts[1]).map_err(|_| denied())?)
        .map_err(|_| denied())?;
    let now = crate::anthropic::unix_now();
    if claims.iss != "teerouter-management"
        || claims.aud != AUDIENCE
        || claims.scope != "handshake"
        || claims.iat > now.saturating_add(60)
        || claims.exp <= now
        || claims.exp <= claims.iat
        || claims.exp - claims.iat > LIFETIME
    {
        return Err(denied());
    }
    let enabled = match claims.role.as_str() {
        "user" => bundle
            .config
            .users
            .iter()
            .any(|u| u.id == claims.sub && u.enabled),
        "merchant" => data.merchants.get(&claims.sub).is_some_and(|m| m.enabled),
        // Operational verification receives the same limited scope, never admin rights.
        "operator" => claims.sub == "operator" && claims.exp - claims.iat <= 600,
        _ => false,
    };
    if enabled { Ok(()) } else { Err(denied()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn signed(claims: serde_json::Value, key: &str) -> String {
        let message = format!(
            "{}.{}",
            B64.encode(br#"{"alg":"HS256","typ":"JWT"}"#),
            B64.encode(serde_json::to_vec(&claims).unwrap())
        );
        format!(
            "{message}.{}",
            B64.encode(
                hmac::sign(
                    &hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()),
                    message.as_bytes()
                )
                .as_ref()
            )
        )
    }
    fn claims() -> serde_json::Value {
        let now = crate::anthropic::unix_now();
        json!({"iss":"teerouter-management","aud":AUDIENCE,"scope":"handshake","role":"user","sub":"alice","iat":now,"exp":now+3600})
    }
    fn fixture() -> std::sync::Arc<Vault> {
        let vault = Vault::memory().unwrap();
        vault.provision(&serde_json::from_value(json!({"config":{"server":{"admin_key_ref":"admin","allow_empty_routes":true,"router_access_key_ref":"access"},"users":[{"id":"alice"}],"keys":[],"models":[]},"secrets":{"admin":"admin-secret","access":KEY}})).unwrap()).unwrap();
        vault
    }
    fn headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HEADER, token.parse().unwrap());
        h
    }
    #[test]
    fn scoped_tokens_validate_locally_and_respect_live_disabled_state() {
        let v = fixture();
        let c = claims();
        let t = signed(c.clone(), KEY);
        assert!(authorize(&v, &headers(&t)).is_ok());
        assert!(authorize(&v, &HeaderMap::new()).is_err());
        assert!(authorize(&v, &headers(&signed(c.clone(), "wrong-key"))).is_err());
        for (k, x) in [
            ("aud", json!("business")),
            ("scope", json!("admin")),
            ("role", json!("admin")),
            ("sub", json!("unknown")),
            ("exp", json!(1)),
            ("iat", json!(crate::anthropic::unix_now() + 120)),
        ] {
            let mut bad = c.clone();
            bad[k] = x;
            assert!(authorize(&v, &headers(&signed(bad, KEY))).is_err(), "{k}");
        }
        let mut duplicate = headers(&t);
        duplicate.append(HEADER, t.parse().unwrap());
        assert!(authorize(&v, &duplicate).is_err());
        v.lock().unwrap().bundle.as_mut().unwrap().config.users[0].enabled = false;
        assert!(authorize(&v, &headers(&t)).is_err());
        let mut merchant = claims();
        merchant["role"] = json!("merchant");
        merchant["sub"] = json!("m_fixture");
        let merchant_token = signed(merchant, KEY);
        assert!(authorize(&v, &headers(&merchant_token)).is_err());
        v.lock().unwrap().merchants.insert("m_fixture".into(),serde_json::from_value(json!({"id":"m_fixture","credential_hash":"a".repeat(64),"public_key":"b".repeat(64),"name":"fixture","enabled":true,"max_concurrent":1,"rpm":0,"tpm":0})).unwrap());
        assert!(authorize(&v, &headers(&merchant_token)).is_ok());
        v.lock()
            .unwrap()
            .merchants
            .get_mut("m_fixture")
            .unwrap()
            .enabled = false;
        assert!(authorize(&v, &headers(&merchant_token)).is_err());
    }
    #[tokio::test]
    async fn all_handshake_routes_require_auth_before_parsing_or_reporting() {
        let v = fixture();
        let engine = crate::router::Engine::from_vault(v.clone()).unwrap();
        let app = crate::attestation::Attestation::new(false, v)
            .unwrap()
            .app(crate::server::app(engine));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for (method, path) in [
            ("GET", "/health/live"),
            ("GET", "/health/ready"),
            ("POST", "/.well-known/teellm/attestation"),
        ] {
            for token in [None, Some("forged")] {
                let mut request = http
                    .request(method.parse().unwrap(), format!("{base}{path}"))
                    .header("content-type", "application/json");
                if let Some(t) = token {
                    request = request.header(HEADER, t);
                }
                let response = request.body("invalid json").send().await.unwrap();
                assert_eq!(response.status(), 401, "{path}");
            }
        }
        let t = signed(claims(), KEY);
        let response = http
            .get(format!("{base}/health/live"))
            .header(HEADER, &t)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let response = http
            .post(format!("{base}/.well-known/teellm/attestation"))
            .header(HEADER, &t)
            .json(&json!({"nonce":"invalid"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        for path in ["/admin/status", "/merchant/state", "/v1/models"] {
            let response = http
                .get(format!("{base}{path}"))
                .header(HEADER, &t)
                .bearer_auth(&t)
                .send()
                .await
                .unwrap();
            assert!(!response.status().is_success(), "{path}");
        }
        task.abort();
    }
}
