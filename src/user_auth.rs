//! Client-owned signing identity and bounded replay protection, shared across config generations.
use crate::{
    error::ApiError,
    request_proof::{self as wire, unhex},
};
use axum::http::{HeaderMap, Method, Uri};
use ring::signature::{ED25519, UnparsedPublicKey};
use std::{collections::HashMap, sync::Mutex};
#[derive(Default)]
pub(crate) struct Replay {
    nonces: Mutex<(u64, HashMap<(String, String), u64>)>,
}
impl Replay {
    pub(crate) fn verify(
        &self,
        boot: &str,
        uid: &str,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<(), ApiError> {
        let denied = || {
            ApiError::new(
                401,
                "user_signature_invalid",
                "Valid client-owned request signature required",
            )
        };
        let field = |name: &str| -> Result<&str, ApiError> {
            if headers.get_all(name).iter().count() != 1 {
                return Err(denied());
            }
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(denied)
        };
        if headers.get_all("connection").iter().any(|value| {
            value.to_str().map_or(true, |s| {
                s.split(',').any(|t| {
                    !matches!(
                        t.trim().to_ascii_lowercase().as_str(),
                        "close" | "keep-alive"
                    )
                })
            })
        }) {
            return Err(denied());
        }
        let public = unhex(field("x-router-user-public-key")?, 32).map_err(|_| denied())?;
        if wire::user_id(&public) != uid {
            return Err(denied());
        }
        let time = field("x-router-user-timestamp")?;
        let timestamp: u64 = time.parse().map_err(|_| denied())?;
        let now = crate::anthropic::unix_now();
        if timestamp.to_string() != time
            || timestamp > now.saturating_add(60)
            || now > timestamp.saturating_add(300)
        {
            return Err(denied());
        }
        let nonce = field("x-router-user-nonce")?;
        unhex(nonce, 32).map_err(|_| denied())?;
        let signature = unhex(field("x-router-user-signature")?, 64).map_err(|_| denied())?;
        UnparsedPublicKey::new(&ED25519, &public)
            .verify(
                &wire::message(boot, uid, method, uri, headers, body, time, nonce),
                &signature,
            )
            .map_err(|_| denied())?;
        let mut cache = self.nonces.lock().unwrap();
        if cache.0 != now {
            cache.1.retain(|_, until| *until >= now);
            cache.0 = now;
        }
        let entries = &mut cache.1;
        let key = (uid.to_owned(), nonce.to_owned());
        if entries.contains_key(&key) {
            return Err(ApiError::new(
                401,
                "user_request_replayed",
                "Request signature already consumed",
            ));
        }
        if entries.len() >= 100_000 {
            return Err(ApiError::new(
                503,
                "user_auth_busy",
                "Request replay protection capacity reached",
            ));
        }
        entries.insert(key, timestamp.saturating_add(300));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_proof as wire;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    fn proof(
        key: &Ed25519KeyPair,
        boot: &str,
        body: &[u8],
        nonce: &str,
    ) -> (String, Method, Uri, HeaderMap) {
        let uid = wire::user_id(key.public_key().as_ref());
        let method = Method::POST;
        let uri = "/v1/responses?test=1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer platform-issued".parse().unwrap());
        headers.insert("x-router-routing", "fixed-pool".parse().unwrap());
        headers.insert("user-agent", "codex_cli_rs/1.0".parse().unwrap());
        let time = crate::anthropic::unix_now().to_string();
        let sig = key.sign(&wire::message(
            boot, &uid, &method, &uri, &headers, body, &time, nonce,
        ));
        for (name, value) in [
            (
                "x-router-user-public-key",
                wire::hex(key.public_key().as_ref()),
            ),
            ("x-router-user-timestamp", time),
            ("x-router-user-nonce", nonce.into()),
            ("x-router-user-signature", wire::hex(sig.as_ref())),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        (uid, method, uri, headers)
    }
    #[test]
    fn owner_proof_covers_identity_boot_body_headers_path_and_replay() {
        let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let (uid, method, uri, headers) = proof(&key, "boot-1", b"{input}", &"a".repeat(64));
        let verifier = Replay::default();
        assert!(
            verifier
                .verify("boot-2", &uid, &method, &uri, &headers, b"{input}")
                .is_err()
        );
        assert!(
            verifier
                .verify("boot-1", &uid, &method, &uri, &headers, b"{changed}")
                .is_err()
        );
        assert!(
            verifier
                .verify(
                    "boot-1",
                    &uid,
                    &method,
                    &"/v1/messages".parse().unwrap(),
                    &headers,
                    b"{input}"
                )
                .is_err()
        );
        assert!(
            verifier
                .verify("boot-1", "victim", &method, &uri, &headers, b"{input}")
                .is_err()
        );
        for name in ["authorization", "x-router-routing", "user-agent"] {
            let mut changed = headers.clone();
            changed.insert(name, "modified".parse().unwrap());
            assert!(
                verifier
                    .verify("boot-1", &uid, &method, &uri, &changed, b"{input}")
                    .is_err()
            );
        }
        let mut duplicate = headers.clone();
        duplicate.append("x-router-user-nonce", "a".repeat(64).parse().unwrap());
        assert!(
            verifier
                .verify("boot-1", &uid, &method, &uri, &duplicate, b"{input}")
                .is_err()
        );
        assert!(
            verifier
                .verify("boot-1", &uid, &method, &uri, &headers, b"{input}")
                .is_ok()
        );
        assert!(
            verifier
                .verify("boot-1", &uid, &method, &uri, &headers, b"{input}")
                .is_err()
        );
        let other = Ed25519KeyPair::from_seed_unchecked(&[8; 32]).unwrap();
        let (_, m, u, h) = proof(&other, "boot-1", b"{input}", &"b".repeat(64));
        assert!(
            verifier
                .verify("boot-1", &uid, &m, &u, &h, b"{input}")
                .is_err()
        );
    }
}
