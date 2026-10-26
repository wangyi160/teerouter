//! OAuth ports from LiteLLM 98465fe2.
//! Per-function origins and deliberate integration changes are recorded in UPSTREAM.json.

pub(crate) mod chatgpt;
mod claude;
pub(crate) mod tokens;

pub(crate) use claude::apply_claude_oauth;
pub(crate) use tokens::TokenStore;

pub(crate) const CHATGPT_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
