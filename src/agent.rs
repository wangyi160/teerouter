//! Client compatibility policy, not proof of the executable sending a request.
//! Never infer an agent from the requested model, API format, or prompt text.
use crate::{Provider, error::ApiError};
use reqwest::header::HeaderMap;

pub(crate) const HEADERS: &[&str] = &[
    "user-agent",
    "originator",
    "x-app",
    "x-grok-client-identifier",
    "x-grok-client-version",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Agent {
    Codex,
    ClaudeCode,
    GrokBuild,
    Other,
}

impl Agent {
    pub fn detect(headers: &HeaderMap) -> Result<Self, ApiError> {
        let ua = single(headers, "user-agent")?.unwrap_or("");
        let origin = single(headers, "originator")?;
        let app = single(headers, "x-app")?;
        let grok = single(headers, "x-grok-client-identifier")?;
        // Match product tokens outside comments. Substring matches would accept
        // browser descriptions, e.g. Mozilla/5.0 (compatible; claude-cli/1.0).
        let products = products(ua)?;
        // Desktop originators contain a space, unlike the CLI/IDE products.
        let desktop = ua
            .split_once('/')
            .filter(|(p, v)| p.starts_with("Codex ") && codex_origin(p) && !v.is_empty())
            .map(|(p, _)| p);
        let codex = products.iter().any(|(p, _)| codex_origin(p)) || desktop.is_some();
        let claude = products.iter().any(|(p, _)| *p == "claude-cli");
        let xai = products.iter().any(|(p, _)| *p == "grok-shell");
        if [codex, claude, xai].into_iter().filter(|v| *v).count() > 1 {
            return Err(invalid());
        }
        let agent = match (codex, claude, xai) {
            (true, false, false) => Self::Codex,
            (false, true, false) => Self::ClaudeCode,
            (false, false, true) => Self::GrokBuild,
            _ => Self::Other,
        };
        if origin.is_some_and(|v| {
            agent != Self::Codex
                || !codex_origin(v)
                || !(products.iter().any(|(p, _)| *p == v) || desktop == Some(v))
        }) || app.is_some_and(|v| agent != Self::ClaudeCode || v != "cli")
            || grok.is_some_and(|v| {
                agent != Self::GrokBuild || !matches!(v, "grok-shell" | "grok-pager")
            })
        {
            return Err(invalid());
        }
        Ok(agent)
    }

    pub fn allows(self, provider: Provider) -> bool {
        // Workbuddy is a separately enabled laboratory backend, never an
        // official provider fallback. It intentionally remains usable by test clients.
        provider.is_local_test()
            || matches!(
                (self, provider),
                (Self::Codex, Provider::OpenAI)
                    | (Self::ClaudeCode, Provider::Anthropic)
                    | (Self::GrokBuild, Provider::XAI)
            )
    }

    pub fn check(self, provider: Provider) -> Result<(), ApiError> {
        if self.allows(provider) {
            Ok(())
        } else {
            Err(self.rejection())
        }
    }

    pub fn rejection(self) -> ApiError {
        if self == Self::Other {
            ApiError::new(
                403,
                "unsupported_agent",
                "Only Codex, Claude Code and Grok Build may use official model services",
            )
        } else {
            ApiError::new(
                403,
                "agent_provider_mismatch",
                "Agent and model provider must match: Codex/OpenAI, Claude Code/Claude, Grok Build/Grok",
            )
        }
    }

    pub fn check_model(self, alias: &str) -> Result<(), ApiError> {
        let model = alias
            .split_once('/')
            .filter(|(p, _)| p.starts_with("m_"))
            .map_or(alias, |(_, m)| m);
        if let Some(provider) = Provider::split_model(model).0 {
            self.check(provider)?;
        }
        Ok(())
    }
}

fn invalid() -> ApiError {
    ApiError::new(
        400,
        "invalid_agent_identity",
        "Client identification headers are malformed or conflicting",
    )
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ApiError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .map(|v| v.to_str().map_err(|_| invalid()))
        .transpose()?;
    // Commas are valid inside UA comments (Claude uses "external, cli");
    // products() rejects them outside comments, including coalesced UA values.
    if values.next().is_some()
        || value.is_some_and(|v| {
            v.is_empty() || v.len() > 1024 || (name != "user-agent" && v.contains(','))
        })
    {
        return Err(invalid());
    }
    Ok(value)
}

fn codex_origin(value: &str) -> bool {
    matches!(
        value,
        "codex_cli_rs" | "codex_exec" | "codex-tui" | "codex_vscode"
    ) || (value.starts_with("Codex ")
        && value[6..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" ._-".contains(&b)))
}

fn products(ua: &str) -> Result<Vec<(&str, &str)>, ApiError> {
    let mut depth = 0usize;
    let mut escaped = false;
    let mut start = None;
    let mut out = Vec::new();
    for (i, ch) in ua.char_indices().chain(std::iter::once((ua.len(), ' '))) {
        if depth > 0 {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => (),
            }
            continue;
        }
        if ch == ')' {
            return Err(invalid());
        }
        if ch.is_ascii_whitespace() || ch == '(' {
            if let Some(a) = start.take() {
                if ua[a..i].contains(',') {
                    return Err(invalid());
                }
                if let Some((p, v)) = ua[a..i].split_once('/') {
                    if !v.is_empty()
                        && v.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b".-_+".contains(&c))
                    {
                        out.push((p, v));
                    }
                }
            }
            if ch == '(' {
                depth = 1;
            }
        } else {
            start.get_or_insert(i);
        }
    }
    if depth != 0 {
        return Err(invalid());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(rows: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (n, v) in rows {
            h.append(
                reqwest::header::HeaderName::from_bytes(n.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }
    #[test]
    fn official_agents_and_provider_matrix() {
        for (ua, extra, agent, provider) in [
            (
                "codex_cli_rs/0.114.0 (Windows 11; x86_64)",
                ("originator", "codex_cli_rs"),
                Agent::Codex,
                Provider::OpenAI,
            ),
            (
                "codex_vscode/0.114.0 (Windows 11; x86_64)",
                ("originator", "codex_vscode"),
                Agent::Codex,
                Provider::OpenAI,
            ),
            (
                "claude-cli/2.1.0 (external, cli)",
                ("x-app", "cli"),
                Agent::ClaudeCode,
                Provider::Anthropic,
            ),
            (
                "grok-pager/0.1.42 grok-shell/0.1.42 (windows; x86_64)",
                ("x-grok-client-identifier", "grok-pager"),
                Agent::GrokBuild,
                Provider::XAI,
            ),
            (
                "grok-shell/0.1.42 (linux; x86_64)",
                ("x-grok-client-identifier", "grok-shell"),
                Agent::GrokBuild,
                Provider::XAI,
            ),
        ] {
            assert_eq!(
                Agent::detect(&headers(&[("user-agent", ua), extra])).unwrap(),
                agent
            );
            for p in [Provider::OpenAI, Provider::Anthropic, Provider::XAI] {
                assert_eq!(agent.allows(p), p == provider);
            }
            assert!(
                agent
                    .check_model(&format!("m_test/{}/model", provider.model_namespace()))
                    .is_ok()
            );
        }
    }
    #[test]
    fn unknown_ambiguous_and_forged_substrings_fail_closed() {
        for ua in [
            "",
            "python-httpx/0.28.1",
            "GitHubCopilotChat/1.0",
            "OpenAI/Python 1.0",
            "not-codex_cli_rs/1.0",
            "Mozilla/5.0 (claude-cli/2.1.0)",
        ] {
            let h = if ua.is_empty() {
                headers(&[])
            } else {
                headers(&[("user-agent", ua)])
            };
            assert_eq!(Agent::detect(&h).unwrap(), Agent::Other);
        }
        for rows in [
            vec![
                ("user-agent", "codex_cli_rs/1.0"),
                ("user-agent", "codex_cli_rs/1.0"),
            ],
            vec![("user-agent", "codex_cli_rs/1.0 grok-shell/1.0")],
            vec![
                ("user-agent", "claude-cli/2.1.0"),
                ("originator", "codex_cli_rs"),
            ],
            vec![("user-agent", "codex_cli_rs/1.0"), ("x-app", "cli")],
            vec![("user-agent", "codex_cli_rs/1.0, grok-shell/1.0")],
            vec![
                ("user-agent", "grok-shell/1.0"),
                ("x-grok-client-identifier", "other"),
            ],
        ] {
            assert!(Agent::detect(&headers(&rows)).is_err());
        }
        assert_eq!(
            Agent::Other.check(Provider::OpenAI).unwrap_err().code,
            "unsupported_agent"
        );
        assert_eq!(
            Agent::GrokBuild
                .check_model("chatgpt/gpt-test")
                .unwrap_err()
                .code,
            "agent_provider_mismatch"
        );
    }
}
