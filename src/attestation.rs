//! TLS and hardware evidence in the router process. dstack builds only use the
//! measured guest's fixed Unix socket; no remote attester or simulator fallback.
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    response::{IntoResponse, Response},
    routing::post,
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, time::timeout};

pub struct Attestation {
    certificate: Vec<u8>,
    tls: Arc<rustls::ServerConfig>,
    sample: bool,
    reports: Arc<Semaphore>,
}
impl Attestation {
    pub fn new(sample: bool) -> Result<Arc<Self>, String> {
        if sample && !cfg!(feature = "sample-demo") {
            return Err("sample evidence requires a development build".into());
        }
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()])
                .map_err(|_| "certificate generation failed")?;
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| "TLS setup failed")?
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        )
        .map_err(|_| "TLS certificate setup failed")?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(Self {
            certificate: cert.der().to_vec(),
            tls: Arc::new(tls),
            sample,
            reports: Arc::new(Semaphore::new(4)),
        }))
    }
    pub fn app(self: &Arc<Self>, router: Router) -> Router {
        router.merge(
            Router::new()
                .route("/.well-known/teellm/attestation", post(attest))
                .layer(DefaultBodyLimit::max(1024))
                .with_state(self.clone()),
        )
    }
    pub async fn serve(self: Arc<Self>, listener: TcpListener, app: Router) -> Result<(), String> {
        let acceptor = tokio_rustls::TlsAcceptor::from(self.tls.clone());
        let slots = Arc::new(Semaphore::new(256));
        loop {
            let (socket, _) = listener.accept().await.map_err(|_| "TLS listener failed")?;
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let Ok(Ok(tls)) = timeout(Duration::from_secs(10), acceptor.accept(socket)).await
                else {
                    return;
                };
                let _ = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(Duration::from_secs(15))
                    .serve_connection(TokioIo::new(tls), TowerToHyperService::new(app))
                    .await;
            });
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    nonce: String,
}
fn binding(nonce: &str, certificate: &[u8]) -> Option<[u8; 64]> {
    if nonce.len() != 64
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&nonce[i * 2..i * 2 + 2], 16).ok()?;
    }
    let mut hash = Sha512::new();
    hash.update(b"teellm:attested-channel:v1\0");
    hash.update(bytes);
    hash.update(Sha256::digest(certificate));
    Some(hash.finalize().into())
}
#[cfg(not(feature = "dstack-tdx"))]
fn hardware_report(binding: [u8; 64]) -> Result<Value, ()> {
    #[cfg(target_os = "linux")]
    {
        use sev::{
            firmware::guest::{AttestationReport, Firmware},
            parser::ByteParser,
        };
        let bytes = Firmware::open()
            .map_err(|_| ())?
            .get_report(None, Some(binding), Some(0))
            .map_err(|_| ())?;
        let report = AttestationReport::from_bytes(&bytes).map_err(|_| ())?;
        if report.report_data != binding {
            return Err(());
        }
        // The client independently downloads and verifies AMD endorsement collateral.
        Ok(json!({"attestation_report":report,"cert_chain":null}))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = binding;
        Err(())
    }
}
#[cfg(feature = "dstack-tdx")]
async fn dstack_report(binding: [u8; 64]) -> Result<Value, ()> {
    #[cfg(unix)]
    {
        // Keep the transport fixed. Environment proxy/simulator variables cannot
        // redirect hardware evidence to a service controlled by the operator.
        let client = reqwest::Client::builder()
            .unix_socket("/var/run/dstack.sock")
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| ())?;
        let data: String = binding.iter().map(|b| format!("{b:02x}")).collect();
        let mut response = client
            .post("http://localhost/GetQuote")
            .json(&json!({"report_data": data}))
            .send()
            .await
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
            if bytes.len() + chunk.len() > 512 * 1024 {
                return Err(());
            }
            bytes.extend_from_slice(&chunk);
        }
        let evidence: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        if evidence.get("quote").and_then(Value::as_str).is_none()
            || evidence.get("event_log").is_none()
        {
            return Err(());
        }
        Ok(evidence)
    }
    #[cfg(not(unix))]
    {
        let _ = binding;
        Err(())
    }
}
async fn attest(
    State(state): State<Arc<Attestation>>,
    Json(challenge): Json<Challenge>,
) -> Response {
    let Some(binding) = binding(&challenge.nonce, &state.certificate) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let evidence = if state.sample {
        #[cfg(feature = "sample-demo")]
        {
            use base64::Engine;
            Ok(
                json!({"svn":"1","report_data":base64::engine::general_purpose::STANDARD.encode(binding)}),
            )
        }
        #[cfg(not(feature = "sample-demo"))]
        {
            Err(())
        }
    } else {
        let Ok(permit) = state.reports.clone().try_acquire_owned() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        #[cfg(feature = "dstack-tdx")]
        {
            let _permit = permit;
            dstack_report(binding).await
        }
        #[cfg(not(feature = "dstack-tdx"))]
        {
            // A timed-out ioctl keeps its permit until it returns, bounding blocked workers.
            let task = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                hardware_report(binding)
            });
            timeout(Duration::from_secs(10), task)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(Err(()))
        }
    };
    match evidence {
        Ok(evidence)=>([("cache-control","no-store")],Json(json!({"protocol_version":1,"evidence_type":if state.sample {"sample"} else if cfg!(feature="dstack-tdx") {"dstack_tdx"} else {"snp"},"nonce":challenge.nonce,"certificate_sha256":format!("{:x}",Sha256::digest(&state.certificate)),"evidence":evidence}))).into_response(),
        Err(())=>(axum::http::StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":{"type":"attestation_unavailable","message":"Hardware attestation is unavailable on this host"}}))).into_response(),
    }
}

/// Dedicated domain-separated report data for the consumption signing key.
/// No caller-supplied digest signing and no simulator fallback.
pub(crate) async fn receipt_evidence(binding: [u8; 64]) -> Result<Value, ()> {
    static REPORTS: Semaphore = Semaphore::const_new(2);
    let permit = REPORTS.try_acquire().map_err(|_| ())?;
    #[cfg(feature = "dstack-tdx")]
    {
        let _permit = permit;
        dstack_report(binding).await
    }
    #[cfg(not(feature = "dstack-tdx"))]
    {
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hardware_report(binding)
        });
        timeout(Duration::from_secs(10), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(Err(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_binds_nonce_and_certificate_and_rejects_invalid_input() {
        assert!(binding("bad", b"cert").is_none());
        let a = binding(&"a".repeat(64), b"cert").unwrap();
        assert_ne!(a, binding(&"b".repeat(64), b"cert").unwrap());
        assert_ne!(a, binding(&"a".repeat(64), b"other").unwrap());
    }
}
