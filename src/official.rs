//! The vendor trust boundary. There is no runtime configuration, environment or CLI override.
use crate::{Error, Provider};

const HOSTS: &[(Provider, &[&str])] = &[
    (
        Provider::OpenAI,
        &["api.openai.com", "chatgpt.com", "auth.openai.com"],
    ),
    (
        Provider::Anthropic,
        &["api.anthropic.com", "console.anthropic.com", "claude.ai"],
    ),
    (Provider::XAI, &["api.x.ai", "auth.x.ai"]),
];

pub(crate) fn validate(value: &str, provider: Option<Provider>) -> Result<(), Error> {
    let url =
        reqwest::Url::parse(value).map_err(|_| Error::InvalidRequest("invalid vendor URL"))?;
    #[cfg(test)]
    if fixture(&url) {
        return Ok(());
    }
    strict(&url, provider)
}

fn strict(url: &reqwest::Url, provider: Option<Provider>) -> Result<(), Error> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default() != Some(443)
        || !HOSTS.iter().any(|(p, hosts)| {
            provider.is_none_or(|expected| expected == *p)
                && url.host_str().is_some_and(|host| hosts.contains(&host))
        })
    {
        return Err(Error::InvalidRequest(
            "upstream must use a compiled-in official HTTPS host on port 443",
        ));
    }
    Ok(())
}

// Only the Rust test harness can reach plain HTTP fixtures. Never compiled into
// either the debug or release executable.
#[cfg(test)]
fn fixture(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some_and(|host| {
            host == "127.0.0.1"
                || host == "[::1]"
                || host == "fixture.invalid"
                || host.ends_with(".fixture.invalid")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_hosts_are_exact_https_and_provider_bound() {
        for (provider, hosts) in HOSTS {
            for host in *hosts {
                let url = reqwest::Url::parse(&format!("https://{host}/future/v2?version=1.1.1"))
                    .unwrap();
                assert!(strict(&url, Some(*provider)).is_ok());
                assert!(strict(&url, None).is_ok());
            }
        }
        for value in [
            "http://api.openai.com/v1",
            "https://api.openai.com:8443/v1",
            "https://api.openai.com.evil.invalid/v1",
            "https://evil.api.openai.com/v1",
            "https://api.openai.com@evil.invalid/v1",
            "https://user:pass@api.openai.com/v1",
            "https://api.openai.com/v1#fragment",
            "https://api.openai.com./v1",
            "https://127.0.0.1/v1",
            "https://api.anthropic.com/v1",
        ] {
            assert!(
                strict(&reqwest::Url::parse(value).unwrap(), Some(Provider::OpenAI)).is_err(),
                "{value}"
            );
        }
    }
}
