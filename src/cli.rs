use std::io::{BufRead, IsTerminal};
use teellm::{
    router::Engine,
    server,
    vault::{Provision, Vault},
};
use zeroize::Zeroizing;
const HELP: &str = "teellm - Rust LLM router
teellm [run] --admin-key-stdin [--bind 127.0.0.1:4000] [--tls-bind 0.0.0.0:8443]
       or --admin-key-env (reads TEELLM_ADMIN_KEY, for encrypted container secrets)

Optional: --journal-dir /data (sealed accounting spool, dstack TEE key required).
Supply one admin token line over a
protected stdin pipe. Management initializes state through /admin/initialize,
saves /admin/checkpoint and acknowledges it before business requests are served.
Plain HTTP is restricted to loopback. Use the attested TLS endpoint for TEE access.";
pub async fn run() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // No core files or same-user ptrace access. The production boot policy
        // must additionally exclude privileged shells and swap.
        if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0
            || unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        {
            return Err("cannot disable process dumps".into());
        }
    }
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some_and(|s| s == "run") {
        args.next();
    }
    let mut bind: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();
    let mut stdin = false;
    let mut admin_env = false;
    let mut journal_dir = None;
    let mut tls_bind: std::net::SocketAddr = "0.0.0.0:8443".parse().unwrap();
    #[allow(unused_mut)]
    let mut sample = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--admin-key-stdin" => stdin = true,
            "--admin-key-env" => admin_env = true,
            "--journal-dir" => {
                journal_dir = Some(std::path::PathBuf::from(
                    args.next().ok_or("--journal-dir requires a directory")?,
                ))
            }
            "--tls-bind" => {
                tls_bind = args
                    .next()
                    .ok_or("--tls-bind requires address")?
                    .parse()
                    .map_err(|_| "invalid TLS bind address")?
            }
            #[cfg(feature = "sample-demo")]
            "--sample" => sample = true,
            "--bind" => {
                bind = args
                    .next()
                    .ok_or("--bind requires address")?
                    .parse()
                    .map_err(|_| "invalid bind address")?
            }
            "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("teellm {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => return Err("unknown option; use --help".into()),
        }
    }
    if stdin == admin_env || (stdin && std::io::stdin().is_terminal()) {
        return Err("choose --admin-key-stdin with a protected pipe or --admin-key-env".into());
    }
    if !bind.ip().is_loopback() {
        return Err("plain HTTP must bind loopback".into());
    }
    let admin = if admin_env {
        std::env::var("TEELLM_ADMIN_KEY").map_err(|_| "TEELLM_ADMIN_KEY is required")?
    } else {
        use std::io::Read;
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::stdin()
            .lock()
            .take(4097)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| "cannot read admin key")?;
        if bytes.last() != Some(&b'\n') {
            return Err("admin key requires a terminated line".into());
        }
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| "invalid admin key")?
    };
    if admin.len() < 32
        || admin.len() > 4096
        || reqwest::header::HeaderValue::from_str(&admin).is_err()
    {
        return Err("admin key must contain 32-4096 valid header characters".into());
    }
    let bundle:Provision=serde_json::from_value(serde_json::json!({"config":{"server":{"bind":bind,"admin_key_ref":"admin","allow_empty_routes":true},"keys":[],"models":[]},"secrets":{"admin":admin}})).map_err(|_|"invalid bootstrap")?;
    let vault = if let Some(dir) = journal_dir {
        Vault::persistent(&dir).await
    } else {
        Vault::memory()
    }
    .map_err(|e| e.to_string())?;
    vault.provision(&bundle).map_err(|e| e.to_string())?;
    vault.require_manager();
    let engine = Engine::from_vault(vault).map_err(|_| "invalid bootstrap engine")?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| "cannot bind listener")?;
    let attestation = teellm::attestation::Attestation::new(sample)?;
    let tls = tokio::net::TcpListener::bind(tls_bind)
        .await
        .map_err(|_| "cannot bind TLS listener")?;
    let app = server::app(engine);
    let public = attestation.app(app.clone());
    eprintln!("{{\"event\":\"awaiting_management\"}}");
    tokio::select! {
        result=axum::serve(listener,app).with_graceful_shutdown(shutdown())=>result.map_err(|_|"server failed".into()),
        result=attestation.serve(tls,public)=>result,
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    eprintln!("{{\"event\":\"shutting_down\"}}");
}
