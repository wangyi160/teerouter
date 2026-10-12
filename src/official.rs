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
    (
        Provider::Gemini,
        &[
            "generativelanguage.googleapis.com",
            "cloudcode-pa.googleapis.com",
            "oauth2.googleapis.com",
            "accounts.google.com",
        ],
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
    #[cfg(feature = "workbuddy-lab")]
    if provider.is_none_or(|p| p == Provider::Workbuddy)
        && workbuddy_origin_matches(url)
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && (url.path() == "/v1" || url.path().starts_with("/v1/"))
    {
        return Ok(());
    }
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

#[cfg(feature = "workbuddy-lab")]
fn workbuddy_origin_matches(url: &reqwest::Url) -> bool {
    let Ok(base) = reqwest::Url::parse(Provider::Workbuddy.base_url()) else {
        return false;
    };
    matches!(base.scheme(), "http" | "https")
        && base.host_str().is_some()
        && base.username().is_empty()
        && base.password().is_none()
        && base.query().is_none()
        && base.fragment().is_none()
        && base.path() == "/v1"
        && url.origin() == base.origin()
}

// Only the Rust test harness can reach plain HTTP fixtures. Never compiled into
// either the debug or release executable. The lab feature above admits only its
// one fixed Workbuddy test service, never these arbitrary fixture hosts.
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
    fn test_endpoint_requires_lab_build_and_cannot_replace_official_providers() {
        #[cfg(feature = "workbuddy-lab")]
        let base = Provider::Workbuddy.base_url();
        #[cfg(not(feature = "workbuddy-lab"))]
        let base = "http://127.0.0.1:8001/v1";
        let url = reqwest::Url::parse(&format!("{base}/models")).unwrap();
        assert_eq!(strict(&url, None).is_ok(), cfg!(feature = "workbuddy-lab"));
        for (provider, _) in HOSTS {
            assert!(strict(&url, Some(*provider)).is_err());
        }
        #[cfg(feature = "workbuddy-lab")]
        {
            assert!(strict(&url, Some(Provider::Workbuddy)).is_ok());
            let mut altered = url.clone();
            altered
                .set_port(Some(url.port_or_known_default().unwrap() + 1))
                .unwrap();
            assert!(strict(&altered, Some(Provider::Workbuddy)).is_err());
            altered = url.clone();
            altered.set_path("/admin");
            assert!(strict(&altered, Some(Provider::Workbuddy)).is_err());
            altered = url.clone();
            altered.set_username("user").unwrap();
            assert!(strict(&altered, Some(Provider::Workbuddy)).is_err());
            for value in [
                "http://192.168.1.6:8001/v1/models",
                "http://127.0.0.1:8002/v1/models",
                "http://127.0.0.1:8001/admin",
                "http://user:pass@127.0.0.1:8001/v1/models",
                "https://api.openai.com/v1/models",
            ] {
                assert!(
                    strict(
                        &reqwest::Url::parse(value).unwrap(),
                        Some(Provider::Workbuddy)
                    )
                    .is_err()
                );
            }
        }
    }
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
