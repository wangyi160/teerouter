//! OAuth ports from LiteLLM 98465fe2 and Gemini CLI 44d764ee.
//! Per-function origins and deliberate integration changes are recorded in UPSTREAM.json.

pub(crate) mod bridge;
pub(crate) mod chatgpt;
mod claude;
pub(crate) mod gemini;
mod gemini_bridge;
mod responses_bridge;
pub(crate) mod tokens;

pub(crate) use claude::apply_claude_oauth;
pub(crate) use tokens::TokenStore;

pub(crate) const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const CHATGPT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub(crate) const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
pub(crate) const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub(crate) const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";

// Public installed-application client constant, copied from Gemini CLI oauth2.ts; not a user secret.

pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const GEMINI_CLI_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";

// Request/session IDs still need secure randomness; no authorization flow is involved.
pub(crate) fn random_string() -> Result<String, crate::Error> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| crate::Error::OAuth("secure random generator unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
