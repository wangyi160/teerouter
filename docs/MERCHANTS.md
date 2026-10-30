<a id="router-merchant-protocol"></a>

# Router 商户协议

**简体中文** | [English](MERCHANTS.en.md)

所有上游账号与池都属于隔离的商户 runtime。根 runtime 负责用户授权、商户公开登记和统一消费记录，根配置的 `models`、`accounts` 必须为空。控制接口不能导出商户凭据。

<a id="identity-and-authentication"></a>

## 身份与认证

商户 ID 为 `m_sig_` 加 32 字节公钥的 SHA-256 小写十六进制摘要。接入凭据为 32 字节随机数的 64 字符小写十六进制编码，`credential_hash` 是该凭据文本的 SHA-256；凭据不参与商户 ID 计算。

`PUT /admin/merchants` 需要管理员 Bearer，登记格式为：

```json
{"id":"m_sig_<公钥SHA256>","public_key":"<32字节公钥十六进制>","credential_hash":"<64字符摘要>","name":"商户名称","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"users":["u_sig_<用户公钥SHA256>"]}
```

商户通道另要求独立的 32 字节控制密钥。Bearer 为 `<接入凭据>.<控制密钥>`；`POST /merchant/bind` 首次绑定控制密钥摘要 `owner_hash`，后续请求同时校验两者。登记更新不能替换已绑定的摘要，管理员或用户 Bearer 不能调用商户私有通道。

商户私有请求应在验证 TEE 证据及当前 TLS 绑定的同一连接发送。首次绑定前，持有接入凭据者可能抢先绑定；只有绑定响应与预期控制密钥一致时，才能确认账号秘密将交给预期身份。Router 不提供控制密钥取回接口。

<a id="public-control-and-private-accounts"></a>

## 公开控制与私有账号

| 通道 | 接口 | Router 行为 |
| --- | --- | --- |
| 管理员 | `GET /admin/merchants` | 返回登记与解锁状态 |
| 管理员 | `GET /admin/merchant-views` | 返回已解锁商户的脱敏状态及未解锁 ID |
| 管理员 | `GET /admin/merchants/{mid}/view` | 返回一个商户的脱敏状态 |
| 管理员 | `PUT /admin/merchants/{mid}/config` | 更新公开池策略、倍率和客户限制 |
| 管理员 | `GET /admin/merchants/{mid}/commands/{oid}` | 查询幂等配置命令回执 |
| 商户所有者 | `POST /merchant/bind` | 绑定控制密钥摘要 |
| 商户所有者 | `POST /merchant/unlock` | 创建内存 runtime；请求含 `minimum_revision` |
| 商户所有者 | `PUT /merchant/accounts/sync` | 校验并激活私有账号、池关联及秘密 |
| 商户所有者 | `PUT /merchant/accounts/access` | 条件替换现有 access 凭据 |
| 商户所有者 | `POST /merchant/accounts/test` | 测试临时凭据与代理，不保存配置 |

公开配置命令包含 `operation_id`、`runtime_id` 和 `config`。`config.expected_revision` 提供版本比较；相同操作编号及相同内容幂等，内容冲突返回错误。该通道拒绝 `accounts`、`secrets` 和明文代理，代理只接受已存在秘密的 `[redacted]` 占位；非空 `compatibility` 被拒绝。

公开 `config` 支持 `pools`、`pool_pricing`、`max_concurrent`、`rpm`、`tpm`、`restrictions`、`customer_defaults` 和 `customers`。池策略可按 `allowed_models` 限制开放模型。账号启停可使用只含 `enabled` 的 `account_controls`。具体输入校验在 `src/merchants/control.rs`。

私有账号同步包含 `operation_id`、`runtime_id`、`expected_revision`、`pools`，以及可选 `accounts`、`secrets`、`delete_accounts`、`delete_secrets`、`new_pool_pricing`。首次建立新池可给初始倍率；已有池的倍率、开放模型和客户限制不会被账号同步覆盖。旧 `accounts/prepare` 和 ticket 提交通道已删除。

账号代理通过私有账号同步传入：API key 账号的 `deployments[].proxy`，或 OAuth `accounts` 对象的 `proxy`。只接受已支持的 HTTP(S)、SOCKS5h 或 [VLESS](VLESS.md) 代理；上游地址仍固定为官方域名。账号配置不接受本地 OAuth 文件、自定义上游 URL 或池间 fallback。

<a id="account-tests"></a>

## 账号测试

`POST /merchant/accounts/test` 接收 `pool`、`proxy`、access-only `credential` 或 `api_key`，可选 `model`。未发布池使用 [draft_pool](MERCHANT-POOLS.md)。Router 先读取模型目录，再选择开放且有文本报价的模型发送最多 32 输出 token 的短请求。

成功返回 `ok:true`、`model`、`model_count`、`provider`、`latency_ms`；失败返回 `ok:false`、`stage`、脱敏错误 `code` 及可用的上游状态。只列出模型不算测试成功。总超时 75 秒，每商户同时最多一次、全局最多八次。测试不保存凭据、不续期、不生成用户消费事件；真实上游可能对该请求计费。

<a id="pools-models-and-charges"></a>

## 池、模型与费用

每池只设置 `multiplier_millionths`：`1000000` 为 1×，`200000` 为 0.2×，最大为 1,000,000,000×。输入、输出、缓存和长上下文费用统一乘倍率，最终按协议舍入。非空旧 `prices` 更新被拒绝。

请求开始时冻结 `merchant_pricing` 的基准费率、倍率、版本、来源及规则。缺少倍率、基准价或价格过期时拒绝调用。已有请求不受之后调价影响。字段和边界见 [计费协议](BILLING.md)。

`allowed_models` 属于池，不属于单个账号。省略或 null 允许全部经过官方目录验证的模型；空数组允许零个模型。最多 4096 个不重复的官方原始 ID，不支持通配符。写入不存在的模型不会创造能力。修改只影响之后的准入。

官方目录按 OpenAI、Claude、Grok 共享，各账号的凭据、代理、健康和限额仍独立。默认 `discovery.refresh_secs:300`，后台最多每 30 秒检查一次；刷新总预算默认 15 秒，并发刷新合并。失败可换来源，全部失败或预算内无成功结果时清空对应目录；下一周期重试，不以失效目录接收新请求。同步账号不自动证明其生成能力。

目录状态含 `scope:"provider"`、`provider`、`refresh_secs`，不暴露来源商户或 `source_deployment`。商户状态中的 `catalog.data` 按本商户账号组织，同一提供商引用相同目录证据。

<a id="user-requests-and-routing"></a>

## 用户请求与选路

使用 [请求路由规则 v1](API.md#请求路由规则-v1) 指定固定池或自动匹配、倍率上限与价格/延迟优先。模型命名空间为 `chatgpt/`、`claude/`、`grok/`；Router 先约束提供商，再检查池、授权、能力和价格。上游请求使用官方模型 ID。

不带路由规则时，仍支持 `x-router-pool: m_sig_<公钥SHA256>.池名` 或商户限定模型名。缺少足以确定商户的标识返回 `400 merchant_required`，没有隐式直连账号。旧账号/目录控制路径不提供兼容别名。

<a id="customer-visibility-and-refusal-thresholds"></a>

## 客户可见性与拒答阈值

只有收到上游响应后才建立客户可见关系，包括生成拒答和 HTTP 错误。预授权、读取目录、本地拒绝及未收到响应的连接失败不建立关系。商户状态不返回预授权的 `merchant.users`。

`customer_defaults` 配置默认 `enabled`、`models`、并发、RPM/TPM 和 `refusal_limit`。`customers` 更新已可见客户；省略客户不删除原权限，停用须显式提交 `enabled:false`。客户限额配置不能自行建立可见关系。

拒答阈值默认 0，表示不限；客户值 null 或省略时继承默认值。范围为 0–1,000,000,000。次数严格超过阈值后，新请求返回 `403 merchant_refusal_limit_exceeded`。固定池不能绕过限制，自动规则可选择其他允许的商户。

计数按商户 ID 与用户 ID 归属，覆盖所有 key、池和模型；每次请求最多计一次明确拒答，普通网络错误、429、普通 5xx 不计数。`customer_refusals` 只展示可见客户的 `total`、`count`、`limit` 和 `blocked`。

清零提交 `refusal_baseline:total` 并启用客户；基线单调递增，读取基线之后的新拒答保留。累计总数独立于逐笔队列，启用日志时同一事务持久化。没有日志时，未同步尾部仍有丢失窗口。

<a id="access-token-only-v1-and-recovery"></a>

## Access-token-only v1 与恢复

`merchant.credential_protocol` 为 `access-token-only-v1`。只接受 access token、到期时间和明确支持的元数据；refresh token、空 refresh 字段、嵌套 refresh 字段与未知字段都拒绝。规则覆盖初始化、账号同步、停用账号和临时测试。

增量请求 `PUT /merchant/accounts/access`：

```json
{"runtime_id":"当前商户实例","accounts":{"alias":{"expected_access_sha256":"旧access摘要","credential":{"access_token":"NEW_ACCESS_TOKEN","expires_at":2000000000}}}}
```

旧实例返回 `merchant_runtime_changed`，旧摘要覆盖返回 `account_credential_changed`，非法 refresh 字段返回 `access_token_only`。完全相同的重试不增加账号版本；该接口不改变池、客户或商户配置版本。响应中 `accounts.data[].access_sha256` 仅对所有者可见。

重启后商户私有 runtime 为空，`configuration_required:true` 表示仍需私有配置。公开检查点不含账号秘密，登记与恢复拒绝旧 `backup` 字段。现有 access token 到期或被官方拒绝后停止使用，等待合法的新凭据。

消费在根 runtime 统一形成一份记录，商户不维护第二份待确认队列，见 [商户记账边界](MERCHANT-LEDGER.md)。用户签名格式见 [USER-AUTH](USER-AUTH.md)，完整路径清单见 [API](API.md)。
