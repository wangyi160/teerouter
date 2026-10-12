# Router API


路径相对于 router 地址；业务请求须先由独立客户端验证 TLS / TEE。

用户接口使用用户 API key；管理接口使用独立管理密钥。认证头为 `Authorization: Bearer <KEY>`，兼容 `x-api-key` / `x-goog-api-key`，不接受 URL 中的 key。

`x-router-pool` 选择账号池，`model` 保持目录中的原始 ID。思考参数取目录允许值，省略时不补写。金额和账务属于管理平台。

## 用户调用

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/v1/models` | 按当前 API key 的权限返回已发现模型及思考能力；模型目录与账号池分别读取。 |
| GET | `/v1/pools` | 返回当前 API key 可访问的池、供应商类型和账号运行状态；不返回账号凭证。 |
| GET | `/v1/usage` | 当前 API key 的调用和 token 统计；金额与历史账单由管理平台计算。 |
| POST | `/v1/chat/completions` | messages 请求格式，可使用 stream；思考强度字段为 reasoning_effort。 |
| POST | `/v1/responses` | input 请求格式，可使用 stream；思考强度字段为 reasoning.effort。不支持 background=true。 |
| POST | `/v1/messages` | messages 请求格式，需 max_tokens；thinking 选项以模型目录为准。 |
| POST | `/v1beta/models/{model}:generateContent` | 模型名在 URL 中，请求体使用 contents。 |
| POST | `/v1beta/models/{model}:streamGenerateContent` | 模型名在 URL 中，返回流式数据。 |

## 健康检查与 TEE 验证

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/health/live` | 200 表示进程可响应，不代表可以调用模型。 |
| GET | `/health/ready` | 可接收业务时返回 200，初始化、同步或上游不可用时返回 503。 |
| POST | `/.well-known/teellm/attestation` | 仅在 TLS 入口提供。nonce 为客户端新生成的 32 字节随机数的小写十六进制编码（64 位）；客户端验证硬件签名、发布策略和当前 TLS 证书绑定。 |

## 管理接口

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/admin/status` | 就绪状态、内存存储、初始化状态、boot_id、在途请求和账号健康情况。 |
| GET | `/admin/metrics` | 入口并发、存储、账号、发现与自动维护指标。 |
| GET | `/admin/usage` | 读取用量统计，不包含用户的问题和回答。 |
| GET | `/admin/models` | 按池和账号返回上游模型目录、思考能力与刷新结果。 |
| POST | `/admin/models/refresh` | 从上游重新读取目录。 |
| GET | `/admin/config` | 返回脱敏配置及 revision。 |
| PATCH | `/admin/config` | 带 revision 提交配置补丁；过期 revision 被拒绝。 |
| POST | `/admin/apply` | 管理平台统一下发配置、凭证与账号变更。 |
| GET | `/admin/users` | 用户、所属 API key、并发权限和 moderation 审核统计。 |
| POST | `/admin/users` | 创建用户及其访问权限；金额不属于 router 用户属性。 |
| GET | `/admin/users/{id}` | 读取指定用户，包括违规次数、最近一次违规及最近 20 条审核异常事件。 |
| PUT | `/admin/users/{id}` | 更新指定用户及其权限。 |
| DELETE | `/admin/users/{id}` | 撤销指定用户及其 API key。 |
| GET | `/admin/accounts` | 返回账号状态，不包含凭证。 |
| GET | `/admin/accounts/{name}/credential` | 受管理密钥保护的完整凭证接口；仅用于管理端账号详情。 |
| PUT | `/admin/accounts/{name}` | 导入或更新账号凭证、代理和启用状态。 |
| DELETE | `/admin/accounts/{name}` | 删除指定账号。 |
| POST | `/admin/accounts/{name}/refresh` | 维护接口；正常使用由 router 自动刷新，无需手动操作。 |
| GET | `/admin/checkpoint` | 管理后端用于加密保存配置、凭证与待确认用量；响应含敏感数据。 |
| POST | `/admin/checkpoint/ack` | 管理后端持久保存后确认消费游标，并续期服务租约。 |
| POST | `/admin/initialize` | 绑定本次 boot_id 恢复管理平台保存的快照。 |
| GET | `/admin/logs` | 运行元数据，不是问题与回答内容。 |
| POST | `/admin/maintenance` | 触发内部维护任务；正常使用由 router 自动执行。 |

## 输入审核错误与用户记录

`config.moderation` 及其独立密钥必须由管理员在初始化/恢复时下发；首次空初始化后可通过 `/admin/apply` 下发。未配置时四种推理协议（包括流式）及 `/v1/models` 拒绝服务，`/health/ready` 返回 503，`/admin/status` 的 `moderation_configured` 为 false，管理接口仍可使用。配置齐全后，在首次模型调用之前执行一次 LockLLM 全量文本审核。上游重试复用本次结果；不同客户端请求不做历史缓存。正常通过后沿用原转发和 SSE 行为，不扫描模型输出。

| HTTP | error.code | 行为 |
| --- | --- | --- |
| 403 | `content_policy_violation` | 明确违规，停止转发，用户 blocked_requests 加一 |
| 503 | `moderation_not_configured` | 管理员尚未下发或已移除审核配置；模型服务不可用，不进入扫描、模型计费或用户违规统计 |
| 503 | `moderation_unavailable` | 超时、限流、服务故障、异常/不完整结果或未确认故障拒绝能力；停止转发，unavailable_requests 加一，不记违规 |
| 413 | `moderation_input_too_large` | 全量输入超过本地审核大小限制或服务返回超限；停止转发，rejected_requests 加一 |
| 400 | `moderation_unsupported_content` | 输入包含不可见历史引用或不支持的非文本内容；停止转发，rejected_requests 加一 |

错误沿用 `{"error":{"type":"...","code":"...","message":"..."}}` 格式和 `x-request-id` 响应头。审核拒绝发生在 SSE 开始之前，仍返回普通 JSON 错误。未调用上游时 attempts 和模型 token 用量为零，不记未知模型费用。

管理端通过 `GET /admin/users/{id}` 的 `user.moderation`（或列表中对应用户的 `moderation`）读取：

```json
{
  "allowed_requests": 10,
  "blocked_requests": 1,
  "unavailable_requests": 0,
  "rejected_requests": 0,
  "last_violation": {
    "id": 11,
    "request_id": "req-example",
    "key": "key-1",
    "model": "example-model",
    "created_at": 1791763200,
    "result": {
      "decision": "blocked",
      "reason": "content_policy",
      "categories": ["sexual_content"],
      "scanner_request_id": "scan-example",
      "scanner_status": 403,
      "input_chars": 1000,
      "duration_ms": 180,
      "config_revision": 2
    }
  },
  "recent_events": []
}
```

`recent_events` 使用相同事件结构，保留最近 20 条 blocked/unavailable/rejected 事件，按旧到新排列；上面的空数组仅用于缩短示例。allowed_requests 在模型请求结束/取消后记入，其他结果立即记入。计数按用户 ID 聚合所有 key，轮换或删除 key、用户资料更新、配置热更新以及 ACK 不清空计数。`moderation` 是只读结果字段，不能通过用户 POST/PUT 修改。

每个已执行审核的调用，其 `moderation` 结果也进入 `/admin/logs` 的待同步调用事件和 checkpoint；在扫描前因其他检查失败的事件以及旧版本历史事件中，该字段可为 null，这不表示允许绕过审核。完整历史由外部管理服务随调用事件保存，router 用户记录仅保留累计统计、最近一次违规和最近 20 条异常。事件只含固定原因、归一化类别、时间、模型及请求 ID，不含对话原文、工具内容或供应商的 violation_details/自由文本说明。未知类别统一为 other_policy。
