//! Strict VLESS configuration; secret values never enter diagnostic messages.
mod process;
use crate::Error;
pub(super) use process::{LeaseLayer, bind};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn invalid() -> Error {
    Error::OAuth(
        "invalid VLESS proxy: TLS with TCP or XHTTP is required; check UUID and transport parameters",
    )
}

#[derive(Serialize)]
pub(super) struct Node {
    server: String,
    port: u16,
    uuid: String,
    network: String,
    sni: String,
    host: String,
    path: String,
    mode: String,
    fingerprint: String,
    alpn: Vec<String>,
    flow: String,
}

impl Node {
    pub(super) fn parse(value: &str) -> Result<Self, Error> {
        if value.len() > 8192
            || value.trim() != value
            || value.bytes().any(|b| b.is_ascii_control())
        {
            return Err(invalid());
        }
        let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
        let uuid = url.username();
        if url.scheme() != "vless"
            || url.password().is_some()
            || url.host_str().is_none()
            || url.port().is_none_or(|p| p == 0)
            || !matches!(url.path(), "" | "/")
            || uuid.len() != 36
            || uuid.bytes().enumerate().any(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b != b'-'
                } else {
                    !b.is_ascii_hexdigit()
                }
            })
        {
            return Err(invalid());
        }
        let mut fields = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            if !matches!(
                key.as_ref(),
                "security"
                    | "encryption"
                    | "type"
                    | "sni"
                    | "host"
                    | "path"
                    | "mode"
                    | "fp"
                    | "alpn"
                    | "flow"
            ) || value.bytes().any(|b| b.is_ascii_control())
                || fields
                    .insert(key.into_owned(), value.into_owned())
                    .is_some()
            {
                return Err(invalid());
            }
        }
        let get = |key: &str, default: &str| {
            fields
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_owned())
        };
        if get("security", "") != "tls" || get("encryption", "none") != "none" {
            return Err(invalid());
        }
        let server = url.host_str().unwrap().trim_matches(['[', ']']).to_owned();
        let network = get("type", "tcp");
        let sni = get("sni", &server);
        let host = get("host", "");
        let path = get("path", "/");
        let mode = get("mode", "auto");
        let fingerprint = get("fp", "");
        let flow = get("flow", "");
        let alpn: Vec<_> = get("alpn", "")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !matches!(network.as_str(), "tcp" | "xhttp")
            || !authority(&sni, false)
            || (!host.is_empty() && !authority(&host, true))
            || !path.starts_with('/')
            || path.len() > 2048
            || !matches!(
                mode.as_str(),
                "auto" | "stream-one" | "stream-up" | "packet-up"
            )
            || !matches!(
                fingerprint.as_str(),
                "" | "chrome" | "firefox" | "safari" | "edge" | "randomized"
            )
            || !matches!(flow.as_str(), "" | "xtls-rprx-vision")
            || (network == "xhttp" && !flow.is_empty())
            || alpn.len() > 2
            || alpn
                .iter()
                .any(|p| !matches!(p.as_str(), "h2" | "http/1.1"))
            || (network == "tcp"
                && ["host", "path", "mode"]
                    .iter()
                    .any(|k| fields.contains_key(*k)))
        {
            return Err(invalid());
        }
        Ok(Self {
            server,
            port: url.port().unwrap(),
            uuid: uuid.to_ascii_lowercase(),
            network,
            sni,
            host,
            path,
            mode,
            fingerprint,
            alpn,
            flow,
        })
    }

    pub(super) fn config(&self) -> Value {
        let mut tls = json!({"serverName":self.sni,"allowInsecure":false});
        if !self.fingerprint.is_empty() {
            tls["fingerprint"] = self.fingerprint.clone().into();
        }
        if !self.alpn.is_empty() {
            tls["alpn"] = json!(self.alpn);
        }
        let mut stream = json!({"network":self.network,"security":"tls","tlsSettings":tls});
        if self.network == "xhttp" {
            stream["xhttpSettings"] = json!({"path":self.path,"host":self.host,"mode":self.mode});
        }
        json!({"log":{"access":"none","error":"none","loglevel":"none"},
            "outbounds":[{"tag":"node","protocol":"vless","settings":{"vnext":[{
                "address":self.server,"port":self.port,"users":[{"id":self.uuid,"encryption":"none","flow":self.flow}]
            }]},"streamSettings":stream,"mux":{"enabled":false}}]})
    }
}

fn authority(value: &str, port: bool) -> bool {
    if value.is_empty()
        || value.len() > 253
        || value
            .bytes()
            .any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'\\'))
    {
        return false;
    }
    reqwest::Url::parse(&format!("https://{value}")).is_ok_and(|u| {
        u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
            && (port || !value.contains(':'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = "vless://12345678-1234-1234-1234-123456789abc@node.example:443";
    #[test]
    fn xhttp_settings_are_explicit_and_never_disable_tls_verification() {
        let node = Node::parse(&format!(
            "{BASE}?security=tls&type=xhttp&path=%2Ftunnel&mode=stream-one&sni=cert.example#Display"
        ))
        .unwrap();
        let value = node.config();
        assert_eq!(
            value["outbounds"][0]["streamSettings"]["xhttpSettings"]["mode"],
            "stream-one"
        );
        assert_eq!(
            value["outbounds"][0]["streamSettings"]["xhttpSettings"]["path"],
            "/tunnel"
        );
        assert_eq!(
            value["outbounds"][0]["streamSettings"]["tlsSettings"]["allowInsecure"],
            false
        );
        assert!(value.get("inbounds").is_none());
        assert_eq!(value["outbounds"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn unsupported_or_ambiguous_fields_are_rejected_without_secrets() {
        for query in [
            "",
            "security=none",
            "security=reality",
            "security=tls&allowInsecure=1",
            "security=tls&security=none",
            "security=tls&type=ws",
            "security=tls&path=/ignored",
            "security=tls&type=xhttp&mode=typo",
            "security=tls&type=xhttp&flow=xtls-rprx-vision",
            "security=tls&sni=user@other.example",
            "security=tls&sni=cert.example%0aPRIVATE",
            "security=tls&alpn=h3",
            "security=tls&encryption=PRIVATE",
        ] {
            let error = Node::parse(&format!("{BASE}?{query}"))
                .err()
                .expect("invalid")
                .to_string();
            assert!(!error.contains("12345678") && !error.contains("PRIVATE"));
        }
        for url in [
            "vless://PRIVATE@node.example:443?security=tls",
            "vless://node.example:443?security=tls",
        ] {
            assert!(Node::parse(url).is_err());
        }
    }
    #[test]
    fn labels_and_query_order_do_not_create_different_nodes() {
        let a = Node::parse(&format!(
            "{BASE}?security=tls&type=xhttp&mode=stream-one#one"
        ))
        .unwrap();
        let b = Node::parse(&format!(
            "{BASE}?mode=stream-one&type=xhttp&security=tls#two"
        ))
        .unwrap();
        assert_eq!(a.config(), b.config());
        assert!(Node::parse(&format!("{BASE}?security=tls&flow=xtls-rprx-vision")).is_ok());
    }
}
