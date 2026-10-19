//! Client-owned dstack/TDX appraisal. A quote is never its own allowlist.
//! Intel signatures, revocation/freshness, measured production OS, the complete
//! RTMR3 launch sequence and the current TLS certificate must all agree.
use crate::protocol::{Check, hex};
use anyhow::{Context, Result, ensure};
use dcap_qvl::{
    QuotePolicy,
    collateral::{CollateralClient, INTEL_PCS_URL},
    configs::DefaultConfig,
    http::{HttpClient, HttpResponse},
    quote::TDReport10,
    verify::QuoteVerifier,
};
use serde::Deserialize;
use sha2::{Digest, Sha384};
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const RUNTIME_EVENT: u32 = 0x08000001;
const LIMIT: usize = 512 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u32,
    pub releases: Vec<Release>,
    #[serde(default)]
    pub allow_dynamic_platform: bool,
    #[serde(default)]
    pub allow_cached_keys: bool,
    #[serde(default)]
    pub allow_smt: bool,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub id: String,
    pub mr_td: String,
    pub rtmrs: [String; 3],
    #[serde(default = "event_version_one")]
    pub event_log_version: u32,
    /// Ordered RTMR3 events from the reviewed OS and exact app-compose.
    /// Only instance-id may vary, by using a null payload.
    pub events: Vec<EventRule>,
}
fn event_version_one() -> u32 {
    1
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRule {
    pub event: String,
    pub payload: Option<String>,
}
impl Policy {
    fn quote_policy(&self, now: u64) -> QuotePolicy {
        QuotePolicy::strict(now)
            .allow_dynamic_platform(self.allow_dynamic_platform)
            .allow_cached_keys(self.allow_cached_keys)
            .allow_smt(self.allow_smt)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        ensure!(bytes.len() <= LIMIT, "TDX 发布策略过大");
        let policy: Self = serde_json::from_slice(&bytes)?;
        policy.validate()?;
        Ok(policy)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.releases.len() <= 64,
            "不支持的 TDX 发布策略"
        );
        let mut ids = BTreeSet::new();
        for release in &self.releases {
            ensure!(
                matches!(release.event_log_version, 1 | 2),
                "不支持的启动事件格式"
            );
            ensure!(
                !release.id.is_empty() && ids.insert(&release.id),
                "发布 ID 为空或重复"
            );
            for mr in std::iter::once(&release.mr_td).chain(release.rtmrs.iter()) {
                let bytes = unhex(mr)?;
                ensure!(
                    bytes.len() == 48 && bytes.iter().any(|b| *b != 0),
                    "OS 测量必须是非零 SHA-384；不能使用占位值"
                );
            }
            ensure!(release.events.len() <= 64, "启动事件过多");
            let mut names = BTreeSet::new();
            for rule in &release.events {
                ensure!(
                    !rule.event.is_empty()
                        && rule
                            .event
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                    "启动事件名必须是 ASCII 小写名称"
                );
                ensure!(names.insert(&rule.event), "不能重复可信启动事件");
                if let Some(payload) = &rule.payload {
                    unhex(payload)?;
                } else {
                    ensure!(
                        rule.event == "instance-id",
                        "只有 instance-id 可使用可变载荷"
                    );
                }
            }
            let compose = release
                .events
                .iter()
                .position(|e| e.event == "compose-hash")
                .context("缺少 compose-hash")?;
            let boundary = release
                .events
                .iter()
                .position(|e| e.event == "boot-mr-done")
                .context("缺少启动边界 boot-mr-done")?;
            ensure!(compose < boundary, "compose-hash 必须在启动边界之前");
            let digest = unhex(
                release.events[compose]
                    .payload
                    .as_deref()
                    .context("缺少容器配置摘要")?,
            )?;
            ensure!(
                digest.len() == 32 && digest.iter().any(|b| *b != 0),
                "无效的容器配置摘要"
            );
            ensure!(
                names.contains(&"key-provider".to_owned()),
                "缺少 key-provider 绑定"
            );
        }
        Ok(())
    }
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.len() <= LIMIT && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "无效的十六进制证据"
    );
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
        .collect()
}

/// dstack GetQuote's event format. The client pins the digest version per release.
#[derive(Clone, Deserialize)]
struct Event {
    imr: u32,
    event_type: u32,
    #[serde(default)]
    digest: String,
    event: String,
    event_payload: String,
    #[serde(default)]
    version: Option<u32>,
}

fn appraise(
    td: &TDReport10,
    events: &[Event],
    release: &Release,
    binding: &[u8; 64],
) -> Result<()> {
    ensure!(
        &td.report_data == binding,
        "REPORT_DATA 与本次挑战/TLS 证书不匹配"
    );
    ensure!(td.td_attributes[0] & 1 == 0, "拒绝 TDX 调试模式");
    ensure!(
        unhex(&release.mr_td)? == td.mr_td,
        "虚拟固件与获准生产版本不符"
    );
    for (expected, actual) in release.rtmrs.iter().zip([td.rt_mr0, td.rt_mr1, td.rt_mr2]) {
        ensure!(
            unhex(expected)? == actual,
            "OS、内核、根文件系统或启动参数不符"
        );
    }
    ensure!(
        events.len() <= 2048 && events.iter().all(|e| e.imr <= 3),
        "无效的测量日志"
    );
    let runtime: Vec<_> = events.iter().filter(|e| e.imr == 3).collect();
    ensure!(
        runtime.len() == release.events.len(),
        "RTMR3 启动事件数量改变"
    );
    let mut mr = [0u8; 48];
    for (event, expected) in runtime.iter().zip(&release.events) {
        ensure!(
            event.version.unwrap_or(1) == release.event_log_version,
            "尚未认可此事件日志版本"
        );
        ensure!(
            event.event_type == RUNTIME_EVENT && event.event == expected.event,
            "启动事件类型或顺序改变"
        );
        let payload = unhex(&event.event_payload)?;
        match &expected.payload {
            Some(value) => ensure!(
                unhex(value)? == payload,
                "启动配置 {} 与本机批准值不符",
                event.event
            ),
            None => ensure!(
                event.event == "instance-id" && matches!(payload.len(), 20 | 32),
                "无效的实例 ID"
            ),
        }
        let digest = if release.event_log_version == 1 {
            let mut hash = Sha384::new();
            hash.update(RUNTIME_EVENT.to_le_bytes());
            hash.update(b":");
            hash.update(event.event.as_bytes());
            hash.update(b":");
            hash.update(&payload);
            hash.finalize()
        } else {
            // JCS: sorted keys, ASCII event name and hex payload, integer type.
            // Policy validation restricts names, avoiding ambiguous escaping.
            let canonical = format!(
                r#"{{"name":"{}","payload":"{}","type":134217729}}"#,
                event.event,
                hex(&payload)
            );
            Sha384::digest(canonical.as_bytes())
        };
        // Production GetQuote may omit this redundant digest (or send "").
        // Always compute it from the event and extend the signed RTMR3 chain;
        // when a digest is included, require it to agree as well.
        if !event.digest.is_empty() {
            ensure!(
                unhex(&event.digest)? == digest.as_slice(),
                "启动事件载荷摘要被修改"
            );
        }
        let mut extension = Sha384::new();
        extension.update(mr);
        extension.update(digest);
        mr = extension.finalize().into();
    }
    ensure!(mr == td.rt_mr3, "事件日志重放结果与硬件签名的 RTMR3 不符");
    Ok(())
}

struct IntelHttp(reqwest::Client);
impl HttpClient for IntelHttp {
    async fn get(&self, target: &str) -> Result<HttpResponse> {
        let mut url = reqwest::Url::parse(target)?;
        ensure!(
            matches!(
                url.host_str(),
                Some("api.trustedservices.intel.com" | "certificates.trustedservices.intel.com")
            ) && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && matches!(url.scheme(), "http" | "https"),
            "拒绝证书指定的非 Intel 下载地址"
        );
        url.set_scheme("https")
            .map_err(|_| anyhow::anyhow!("invalid certificate URL"))?;
        let mut response = self.0.get(url).send().await?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_owned())))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(body.len() + chunk.len() <= LIMIT, "Intel 证书材料响应过大");
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

pub struct TdxVerifier {
    policy: Policy,
    http: IntelHttp,
}
impl TdxVerifier {
    pub fn new(policy: Policy) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            policy,
            http: IntelHttp(
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .build()?,
            ),
        })
    }
    pub async fn verify(
        &self,
        evidence: &serde_json::Value,
        binding: &[u8; 64],
    ) -> Result<Vec<Check>> {
        ensure!(
            !self.policy.releases.is_empty(),
            "TDX 发布允许表为空；不会自动信任服务器的测量值"
        );
        let quote = unhex(evidence["quote"].as_str().context("缺少 TDX quote")?)?;
        ensure!(
            !quote.is_empty() && quote.len() <= 64 * 1024,
            "TDX quote 大小不合法"
        );
        let events: Vec<Event> = match &evidence["event_log"] {
            serde_json::Value::String(value) => serde_json::from_str(value)?,
            value => serde_json::from_value(value.clone())?,
        };
        // Collateral is downloaded from Intel. The server cannot choose the
        // signing root, verification time, certificate URLs or pass/fail result.
        let collateral = CollateralClient::<DefaultConfig, _>::new(&self.http, INTEL_PCS_URL)
            .fetch(&quote)
            .await
            .context("无法取得 Intel 证书/吊销/TCB 材料")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let claims = QuoteVerifier::new_prod()
            .verify_with_policy(&quote, collateral, now, &self.policy.quote_policy(now))
            .context("Intel 签名、吊销状态、证书有效期或 TCB 验证失败")?;
        let td = claims.report.as_td10().context("拒绝非 TDX 硬件证据")?;
        let release = self
            .policy
            .releases
            .iter()
            .find(|release| appraise(td, &events, release, binding).is_ok())
            .context("TDX 证明未匹配任何获准 OS/router/启动策略，或 TLS 挑战绑定不一致")?;
        Ok(vec![
            Check::new(
                "Intel TDX 硬件与证书链",
                "passed",
                "本机验证 Intel 生产根、签名、吊销材料、有效期和严格 TCB 策略；调试模式禁用",
            ),
            Check::new(
                "生产 OS 与启动配置",
                "passed",
                format!("MRTD、RTMR0–2 匹配客户端获准发布：{}", release.id),
            ),
            Check::new(
                "router 与应用配置",
                "passed",
                "完整重放 RTMR3；启动事件顺序、容器配置摘要和密钥提供方均匹配本机策略",
            ),
            Check::new(
                "TDX REPORT_DATA",
                "passed",
                format!("本次 TLS/随机挑战绑定：{}", hex(binding)),
            ),
        ])
    }
}

impl HttpClient for &IntelHttp {
    async fn get(&self, url: &str) -> Result<HttpResponse> {
        IntelHttp::get(self, url).await
    }
}

#[cfg(test)]
#[path = "tdx_tests.rs"]
mod tests;
