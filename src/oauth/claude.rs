//! Extracted from litellm-rust/crates/llms/src/anthropic/common_utils.rs:
//! OauthToken::parse_key, with_oauth_companions and merge_beta_headers.
//! HeaderMap replaces the upstream Headers/BetaSet types; credential behavior is retained.
use crate::Error;
use reqwest::header::{HeaderMap, HeaderValue};
use std::collections::BTreeSet;

pub(crate) fn apply_claude_oauth(headers: &mut HeaderMap, key: &str) -> Result<bool, Error> {
    let token = key.strip_prefix("Bearer ").unwrap_or(key);
    if !token.starts_with("sk-ant-oat") {
        return Ok(false);
    }
    headers.remove("x-api-key");
    let mut credential = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| Error::OAuth("invalid Claude OAuth token header"))?;
    credential.set_sensitive(true);
    headers.insert("authorization", credential);
    let mut betas = BTreeSet::new();
    for value in headers.get_all("anthropic-beta") {
        for beta in value
            .to_str()
            .map_err(|_| Error::InvalidRequest("invalid anthropic-beta header"))?
            .split(',')
        {
            if !beta.trim().is_empty() {
                betas.insert(beta.trim().to_owned());
            }
        }
    }
    betas.insert("oauth-2025-04-20".into());
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_str(&betas.into_iter().collect::<Vec<_>>().join(","))
            .map_err(|_| Error::InvalidRequest("invalid anthropic-beta header"))?,
    );
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        HeaderValue::from_static("true"),
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    // Cases retained from upstream oauth_key_parses_the_token_behind_an_optional_bearer.
    #[rstest]
    #[case("sk-ant-oat01-token", true)]
    #[case("Bearer sk-ant-oat01-token", true)]
    #[case("sk-ant-api03-token", false)]
    #[case("bearer sk-ant-oat01-token", false)]
    #[case("Bearer Bearer sk-ant-oat01-token", false)]
    #[case("SK-ANT-OAT01-token", false)]
    fn token_recognition_and_beta_merge(#[case] key: &str, #[case] expected: bool) {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("old-key"));
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("web-search-2025-03-05, oauth-2025-04-20"),
        );
        assert_eq!(apply_claude_oauth(&mut headers, key).unwrap(), expected);
        if expected {
            assert!(headers.get("x-api-key").is_none());
            assert_eq!(headers["authorization"], "Bearer sk-ant-oat01-token");
            assert!(headers["authorization"].is_sensitive());
            assert_eq!(
                headers["anthropic-beta"],
                "oauth-2025-04-20,web-search-2025-03-05"
            );
            assert_eq!(headers["anthropic-dangerous-direct-browser-access"], "true");
        }
    }
}
