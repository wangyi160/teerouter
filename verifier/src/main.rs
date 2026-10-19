//! Standalone: no Router, portal, teeproxy or teemerchant runtime dependency.
mod protocol;
mod receipt;
mod tdx;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{io::Read, path::Path};

async fn run() -> Result<Value> {
    let mut args = std::env::args().skip(1);
    let mut bundle = None;
    let mut policy_path = None;
    let mut stdin = false;
    let mut signature_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bundle" => bundle = Some(args.next().context("--bundle needs a file")?),
            "--policy" => policy_path = Some(args.next().context("--policy needs a file")?),
            "--stdin" => stdin = true,
            "--signature-only" => signature_only = true,
            "--help" | "-h" => {
                return Ok(
                    json!({"usage":"teerouter-verify --bundle receipt.json --policy trusted-tdx-policy.json",
                "stdin":"--stdin accepts {bundle, policy, expected_nonce?}; policy must be independently trusted",
                "signature_only":"--signature-only checks signatures and inclusion ONLY; does not establish TEE trust"}),
                );
            }
            _ => anyhow::bail!("unknown option"),
        }
    }
    ensure!(stdin != bundle.is_some(), "choose --bundle or --stdin");
    let mut bytes = Vec::new();
    if stdin {
        std::io::stdin()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
    } else {
        std::fs::File::open(bundle.unwrap())?
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
    }
    ensure!(bytes.len() <= 32 * 1024 * 1024, "bundle too large");
    let input: Value = serde_json::from_slice(&bytes)?;
    let (bundle, expected_nonce) = if stdin {
        (&input["bundle"], input["expected_nonce"].as_str())
    } else {
        (&input, None)
    };
    let policy = if signature_only {
        None
    } else if let Some(path) = policy_path {
        Some(tdx::Policy::load(Path::new(&path))?)
    } else if stdin {
        Some(serde_json::from_value::<tdx::Policy>(
            input["policy"].clone(),
        )?)
    } else {
        anyhow::bail!(
            "a separately trusted --policy is required; bundle cannot choose its own trust policy"
        )
    };
    let mut result = json!({"valid":true,"tee_verified":false,"signature_verified":false,
        "scope":"router_consumption_only","complete_history":false,"latest_verified":false});
    let manifest = if !bundle["batch"].is_null() {
        let (manifest, count) = receipt::verify_batch(&bundle["batch"])?;
        result["signature_verified"] = true.into();
        result["records_verified"] = count.into();
        result["batch_hash"] = bundle["batch"]["batch_hash"].clone();
        if let Some(records) = bundle["inclusions"].as_array() {
            ensure!(records.len() <= 8192, "too many inclusion proofs");
            for record in records {
                receipt::verify_inclusion(&manifest, record)?;
            }
            result["inclusions_verified"] = records.len().into();
        }
        if !bundle["previous"].is_null() {
            let (previous, _) = receipt::verify_batch(&bundle["previous"])?;
            receipt::verify_link(&previous, &bundle["previous"]["batch_hash"], &manifest)?;
            result["previous_link_verified"] = true.into();
            // The previous signature is checked, but its hardware identity is not
            // inherited from this boot. Verify its own bundle independently.
            result["previous_tee_verified"] = false.into();
        }
        result["manifest"] = manifest.clone();
        Some(manifest)
    } else {
        None
    };
    if !signature_only {
        let identity = &bundle["identity"];
        let binding = receipt::identity_binding(identity, expected_nonce)?;
        if let Some(manifest) = &manifest {
            ensure!(
                identity["public_key"] == manifest["public_key"]
                    && identity["boot_id"] == manifest["boot_id"],
                "receipt key / boot differs from TEE evidence"
            );
        }
        let verifier = tdx::TdxVerifier::new(policy.context("policy required")?)?;
        result["checks"] =
            serde_json::to_value(verifier.verify(&identity["evidence"], &binding).await?)?;
        result["tee_verified"] = true.into();
    } else {
        ensure!(manifest.is_some(), "signature-only requires a signed batch");
    }
    Ok(result)
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match run().await {
        Ok(result) => println!("{result}"),
        Err(error) => {
            println!(
                "{}",
                json!({"valid":false,"tee_verified":false,"error":error.to_string()})
            );
            std::process::exit(1);
        }
    }
}
