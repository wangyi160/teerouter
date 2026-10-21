//! OAuth ports from LiteLLM 98465fe2.
//! Per-function origins and deliberate integration changes are recorded in UPSTREAM.json.

pub(crate) mod bridge;
pub(crate) mod chatgpt;
mod claude;
mod responses_bridge;
pub(crate) mod tokens;

pub(crate) use claude::apply_claude_oauth;
pub(crate) use tokens::TokenStore;

pub(crate) const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";

// Request/session IDs still need secure randomness; no authorization flow is involved.
pub(crate) fn random_string() -> Result<String, crate::Error> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| crate::Error::OAuth("secure random generator unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
