# Router HTTP API

**简体中文** | [English](API.en.md)

Router 提供 39 个显式方法/路径操作（不计 GET 隐含的 HEAD）。所有操作都需要凭据。本索引只描述 Router 已实现的处理器；控制端点属于 Router 自身的 HTTP 协议。

<a id="model-forwarding"></a>

## 模型转发

| 方法 | 路径 | 认证 |
| --- | --- | --- |
| POST | `/v1/responses` | 用户 Bearer + 客户端 Ed25519 证明 |
| POST | `/v1/messages` | 用户 Bearer + 客户端 Ed25519 证明 |
| POST | `/v1/chat/completions` | 用户 Bearer + 客户端 Ed25519 证明 |

见[用户请求签名](USER-AUTH.md)。转发前检查 Agent/提供商兼容性、权限、路由策略、限额与余额。原生请求参数保持不变，仅将带命名空间的路由模型 ID 解析成对应的上游模型 ID。

OpenAI API key 账号使用 `https://api.openai.com/v1`；ChatGPT OAuth 账号使用 `https://chatgpt.com/backend-api/codex` 和原生 Responses 协议。后者是基于账号的 Codex 通道，与 ChatGPT 网页聊天不同。Router 保留所选通道的请求参数与凭据。

模型请求正文上限 `server.max_body_bytes` 默认为 536,870,912 字节（512 MiB），覆盖截至 2026-10-29 核对的 [OpenAI 视觉 API](https://developers.openai.com/api/docs/guides/images-vision) 所列 512 MB 总载荷范围。上限计算整个序列化 JSON，包括 Base64 图片和对话历史，无论是否提供 `Content-Length` 都生效。显式配置可覆盖默认值。这是 Router 的资源上限，不代表 ChatGPT 账号后端的限额，也不覆盖模型、图片、上下文或其他提供商限制；这些由上游验证。Router 原样转发图片，不缩放或重新压缩。

正文错误分别返回：

| HTTP 状态 | 错误代码 | 含义 |
| --- | --- | --- |
| 413 | `body_too_large` | 接收正文超过 Router 配置上限，未转发。 |
| 400 | `body_read_error` | 读取传入正文失败，未转发。 |
| 408 | `body_timeout` | 接收正文超过 `server.body_timeout_secs`，未转发。 |
| 413 | `upstream_body_too_large` | 提供商返回 HTTP 413；Router 保留状态，不重试请求。 |

并发请求仍共同受入口与生成并发限制约束。上限不会为每个请求预留对应大小的内存；实际收到的正文及解析后的 JSON 在处理时占用内存。

<a id="attestation-and-health"></a>

## 证明与健康状态

| 方法 | 路径 | 认证 |
| --- | --- | --- |
| POST | `/.well-known/teellm/attestation` | `x-router-access` 中的握手 JWT |
| GET | `/health/live` | 握手 JWT 或管理员 Bearer |
| GET | `/health/ready` | 握手 JWT 或管理员 Bearer |

`/health/live` 宣告 `user_auth_version: 2`、当前 `boot_id` 和路由能力。握手 JWT 不能授权模型或管理操作。

<a id="merchant-direct-channel"></a>

## 商户直连通道

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| POST | `/merchant/bind` | 绑定商户本地持有的控制凭据 |
| POST | `/merchant/unlock` | Router 启动后打开商户 runtime |
| PUT | `/merchant/accounts/sync` | 上传账号、access token 和私有代理资料 |
| PUT | `/merchant/accounts/access` | 替换访问令牌资料 |
| POST | `/merchant/accounts/test` | 通过账号代理测试草稿凭据 |

这些操作要求商户接入凭据与商户自持控制凭据，并通过已验证的连接发送。refresh token 被拒绝。公开配置使用管理员认证的控制端点；不存在凭据暂存 ticket 或商户流水查询/ACK 通道。

<a id="router-control-interfaces"></a>

## Router 控制接口

以下操作均要求 Router 管理员 Bearer。该权限不能导出商户凭据或签署用户请求。

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET / PUT | `/admin/merchants` | 商户登记与准入 |
| GET | `/admin/merchant-views` | 商户公开快照 |
| GET | `/admin/merchants/{mid}/view` | 单个商户公开快照 |
| PUT | `/admin/merchants/{mid}/config` | 公开配置命令 |
| GET | `/admin/merchants/{mid}/commands/{oid}` | 确认幂等命令 |
| GET | `/admin/public-catalog` | 只读账号池目录快照 |
| GET / PATCH | `/admin/config` | 读取/更新根配置 |
| POST | `/admin/apply` | 原子更新用户、权限和根配置 |
| POST | `/admin/checkpoint/diff` | 差量检查点；基线缺失或过期时返回完整状态 |
| POST | `/admin/initialize` | 初始化/恢复新启动进程 |
| POST | `/admin/receipts/evidence` | 经证明绑定的流水签名身份 |
| GET / PUT | `/admin/receipts/relay` | 流水上传配置/状态 |
| POST | `/admin/receipts/relay/flush` | 上传待处理签名批次 |
| POST | `/admin/receipts/relay/settle` | 确认已归档批次并应用余额 |
| GET / PUT | `/admin/pricing` | 官方价格簿/带版本价格更新 |
| GET / PUT | `/admin/pricing/source` | 价格源配置 |
| POST | `/admin/pricing/refresh` | 刷新配置的价格源 |
| GET | `/admin/status` | 运行状态 |
| GET | `/admin/usage` | 汇总账务状态 |
| GET | `/admin/logs` | 运行审计记录 |
| GET | `/admin/metrics` | 运行/存储/发现指标 |
| POST | `/admin/quiesce` | 排空/恢复请求 |
| POST | `/admin/maintenance` | 显式存储维护 |

旧版用户查询、用户 CRUD、根上游账号 CRUD/凭据导出、独立拒答队列、完整检查点 GET/ACK、模型发现 GET/刷新路径及其处理器均已移除，不作为兼容别名保留。认证后，未知路径返回 404；保留路径上的不支持方法返回 405。

<a id="request-routing-rules-v1"></a>

## 请求路由规则 v1

`X-Router-Routing` 包含一个最多 2048 字节的 JSON 对象：

```json
{"version":1,"mode":"auto","max_multiplier_millionths":1000000,"priority":"price"}
```

`mode` 为 `auto` 或 `fixed`；`fixed` 必须提供格式为 `m_sig_<公钥SHA256>.池名` 的 `pool`，`auto` 禁止提供该字段。`priority` 为 `price`（默认）或 `speed`。最高倍率以百万分之一为单位的整数表示，上限为 1,000,000,000,000,000。未知字段和重复请求头会被拒绝。

该规则不能与 `x-router-pool`、`x-router-account`、`x-router-price-version` 或 `x-router-official-price-revision` 同用，否则返回 `400 routing_rule_conflict`。选路前检查提供商/Agent 约束、客户授权、池策略、能力证据、报价有效期和并发。价格排序包含适用缓存费用；速度排序使用账号代理的 HTTPS 延迟，而非模型生成吞吐。已有状态上下文保持使用原账号。未知 response 或提供商文件可能需要显式指定原账号池。

未提供规则时，必须由商户限定模型名或 `x-router-pool` 确定商户，否则返回 `400 merchant_required`。详情与隐私边界见[商户协议](MERCHANTS.md)。
