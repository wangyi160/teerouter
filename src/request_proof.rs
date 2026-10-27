//! Ed25519 request proof v1. Shared byte format with teeproxy; no platform secret.
use axum::http::{HeaderMap, Method, Uri};
use sha2::{Digest, Sha256};

pub(crate) const DOMAIN: &str = "teerouter-user-request-v1";
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn unhex(value: &str, length: usize) -> Result<Vec<u8>, &'static str> {
    if value.len() != length * 2
        || !value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
    {
        return Err("invalid encoding");
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).map_err(|_| "invalid encoding"))
        .collect()
}
pub(crate) fn user_id(public: &[u8]) -> String {
    format!("u_sig_{}", hex(&Sha256::digest(public)))
}
fn covered(name: &str) -> bool {
    !name.starts_with("x-router-user-")
        && !matches!(
            name,
            "host"
                | "content-length"
                | "connection"
                | "transfer-encoding"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "proxy-authorization"
                | "proxy-connection"
        )
}
pub(crate) fn message(
    boot: &str,
    uid: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    time: &str,
    nonce: &str,
) -> Vec<u8> {
    let mut names: Vec<_> = headers.keys().filter(|n| covered(n.as_str())).collect();
    names.sort_by_key(|n| n.as_str());
    let fields: Vec<_> = names
        .into_iter()
        .flat_map(|name| {
            headers
                .get_all(name)
                .iter()
                .map(move |value| (name.as_str(), hex(value.as_bytes())))
        })
        .collect();
    serde_json::to_vec(&(
        DOMAIN,
        boot,
        uid,
        method.as_str(),
        uri.path_and_query().map_or("/", |v| v.as_str()),
        time,
        nonce,
        hex(&Sha256::digest(body)),
        fields,
    ))
    .expect("serializable proof")
}
