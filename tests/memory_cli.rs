use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Running {
    child: Child,
    url: String,
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
const ADMIN: &str = "fixture-admin-secret-at-least-32-characters";
async fn start(dir: &std::path::Path) -> Running {
    start_with_source(dir, false).await
}
async fn start_with_source(dir: &std::path::Path, from_env: bool) -> Running {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let mut command = Command::new(env!("CARGO_BIN_EXE_teellm"));
    if from_env {
        command.env("TEELLM_ADMIN_KEY", ADMIN);
    } else {
        command.env_remove("TEELLM_ADMIN_KEY");
    }
    let mut child = command
        .args([
            if from_env {
                "--admin-key-env"
            } else {
                "--admin-key-stdin"
            },
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--tls-bind",
            "127.0.0.1:0",
        ])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    if !from_env {
        writeln!(child.stdin.take().unwrap(), "{ADMIN}").unwrap();
    }
    let running = Running {
        child,
        url: format!("http://127.0.0.1:{port}"),
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    for _ in 0..100 {
        if client
            .get(format!("{}/health/live", running.url))
            .bearer_auth(ADMIN)
            .send()
            .await
            .is_ok()
        {
            return running;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router did not start")
}
#[tokio::test]
async fn encrypted_container_secret_can_boot_an_empty_router() {
    let dir = tempfile::tempdir().unwrap();
    let running = start_with_source(dir.path(), true).await;
    assert_eq!(
        checkpoint(&running).await["initialized"],
        false
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
#[test]
fn missing_invalid_or_ambiguous_admin_secrets_are_rejected_without_disclosure() {
    for (args, secret) in [
        (vec!["--admin-key-env"], None),
        (vec!["--admin-key-env"], Some("short-secret")),
        (vec!["--admin-key-env", "--admin-key-stdin"], Some(ADMIN)),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_teellm"));
        command
            .args(args)
            .env_remove("TEELLM_ADMIN_KEY")
            .stdin(Stdio::null());
        if let Some(secret) = secret {
            command.env("TEELLM_ADMIN_KEY", secret);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        if let Some(secret) = secret {
            assert!(!String::from_utf8_lossy(&result.stderr).contains(secret));
            assert!(!String::from_utf8_lossy(&result.stdout).contains(secret));
        }
    }
}
async fn get(r: &Running, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{}{path}", r.url))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn checkpoint(r:&Running)->Value {
    post(r,"/admin/checkpoint/diff",json!({})).await.json().await.unwrap()
}
async fn post(r: &Running, path: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", r.url))
        .bearer_auth(ADMIN)
        .json(&body)
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn empty_boot_requires_initialization_but_serving_does_not_require_ack() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path()).await;
    let response = reqwest::get(format!("{}/v1/models", first.url))
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let c = checkpoint(&first).await;
    assert_eq!(c["initialized"], false);
    assert_eq!(
        post(
            &first,
            "/admin/initialize",
            json!({"boot_id":"wrong","state":null})
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        post(
            &first,
            "/admin/initialize",
            json!({"boot_id":c["boot_id"],"state":null})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        reqwest::get(format!("{}/v1/models", first.url))
            .await
            .unwrap()
            .status(),
        401
    );
    let key = "sk-fixture-user";
    assert_eq!(post(&first,"/admin/apply",json!({"config":{"users":[{"id":"test-user"}],"keys":[{"name":"client","user_id":"test-user","key_ref":"client","enabled":true}]},"secrets":{"client":key}})).await.status(),200);
    let saved = checkpoint(&first).await;
    // No manager contact for longer than the former lease; initialization remains valid.
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(get(&first, "/admin/status").await["platform_ready"], true);
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/models", first.url))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(post(&first, "/admin/quota", json!({})).await.status(), 404);
    drop(first);
    let second = start(dir.path()).await;
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/models", second.url))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let c = checkpoint(&second).await;
    assert_ne!(c["boot_id"], saved["boot_id"]);
    let mut unowned = saved["state"].clone();
    unowned["bundle"]["config"]["models"] = json!([{"name":"unowned","provider":"openai","deployments":[{"id":"old","provider":"openai","api_key_ref":"unused","enabled":false}]}]);
    let rejected = post(
        &second,
        "/admin/initialize",
        json!({"boot_id":c["boot_id"],"state":unowned}),
    )
    .await;
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "merchant_ownership_required"
    );
    assert_eq!(
        post(
            &second,
            "/admin/initialize",
            json!({"boot_id":c["boot_id"],"state":saved["state"]})
        )
        .await
        .status(),
        200
    );
    let c = checkpoint(&second).await;
    // Refusal auditing needs no external scanner key after checkpoint restore.
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/models", second.url))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let config = get(&second, "/admin/config").await;
    for path in [
        "/admin/accounts",
        "/admin/accounts/old/credential",
        "/admin/models",
    ] {
        assert_eq!(
            reqwest::Client::new()
                .get(format!("{}{path}", second.url))
                .bearer_auth(ADMIN)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    assert!(config["config"]["keys"][0].get("quota_group").is_none());
    assert!(config["config"]["server"].get("database").is_none());
    assert_eq!(
        post(
            &second,
            "/admin/apply",
            json!({"config":{"users":[{"id":"test-user"}],"keys":[{"name":"client","user_id":"test-user","key_ref":"client","enabled":false}]}})
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/v1/models", second.url))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
