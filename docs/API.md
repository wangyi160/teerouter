# Router API


路径相对于 router 地址；业务请求须先由独立客户端验证 TLS / TEE。

用户接口使用用户 API key；管理接口使用独立管理密钥。认证头为 `Authorization: Bearer <KEY>`，兼容 `x-api-key`，不接受 URL 中的 key。

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
| GET | `/admin/users` | 用户、所属 API key、并发权限、refusals 拒答记录及旧 moderation 历史。 |
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
| GET | `/admin/logs` | 待同步调用元数据及拒答证据；不包含请求正文或正常回答。 |
| POST | `/admin/maintenance` | 触发内部维护任务；正常使用由 router 自动执行。 |

## 上游拒答与用户记录

管理员使用 `GET /admin/users/{id}` 读取 `user.refusals`，或通过 `GET /admin/users` 的 `data[].refusals` 读取。普通用户凭证不能访问这些接口，响应使用 `Cache-Control: no-store`。用户 POST/PUT 不接受写入 `refusals`。

```json
{
  "refused_requests": 1,
  "last_refusal_at": 1791763200,
  "evicted_events": 0,
  "recent_events": [{
    "id": 12,
    "request_id": "router-request-id",
    "key": "key-name",
    "model": "requested-model",
    "pool": "chat",
    "deployment": "account-1",
    "created_at": 1791763200,
    "status": 200,
    "attempts": 1,
    "result": {
      "provider": "openai",
      "protocol": "responses",
      "streamed": true,
      "upstream_status": 200,
      "upstream_request_id": "upstream-request-id",
      "response_id": "response-id",
      "response_model": "reported-model",
      "signals": ["response.refusal.done"],
      "reasons": [],
      "parts": [{"output_index": 0, "content_index": 0, "refusal": "Provider refusal text"}],
      "response_text": null,
      "stop_details": null,
      "error": null,
      "complete": true,
      "truncated": false
    }
  }]
}
```

`key` 是密钥名称，不是密钥值。`protocol` 和 `streamed` 描述实际观察的上游协议；Codex 即使客户端请求 JSON，也可能返回 SSE。`complete` 表示是否读到了完整上游响应；`status` 是 router 最终结算状态，例如流中断 502、超时 504、客户端取消 499，`upstream_status` 保留最初上游 HTTP 状态。上游未提供的元数据为 null。`truncated` 标明审计信息触及存储限制，客户端响应不受该限制影响。

| 上游 / 接口 | 明确信号 | 保存内容 |
| --- | --- | --- |
| OpenAI / Grok / Workbuddy Chat | `choices[].message.refusal`、`delta.refusal`、`finish_reason: content_filter` | 合并的拒答原文、命中字段、结束原因、响应标识 |
| OpenAI / Grok Responses（含 Codex 转换） | `type: refusal` 内容块、`response.refusal.delta/done`、`incomplete_details.reason: content_filter` | 按 output/content 索引合并的原文和信号；重复 done/最终快照不重复累计 |
| Claude Messages（含 Chat 转换） | `stop_reason: refusal`、`stop_details.type: refusal` | `stop_details.type/category/explanation` 与同一响应的可见文本；不保存思考内容 |
| 上游明确安全错误 | `error.code/type` 为 `content_policy_violation`、`content_filter`、`safety_violation` | 错误 code/type/message 和原 HTTP 状态；单独的 400/403/429 不算拒答 |

普通 HTTP 200 拒答沿原有协议返回；明确安全错误返回 403 / `upstream_refusal`，不会重试其他账号。流已经开始后的传输错误仍走 SSE `stream_interrupted` 路径，同时保留已观察的拒答。一般的拒答文本、logprobs、工具输出或用户输入不用于推断拒答。Grok/Workbuddy 只在实际返回上述字段时记录，不能保证每次拒答都有结构化标志。

累计按用户 ID 聚合所有 key，请求完成或中断时写入一次。用户/密钥更新、配置热更新和 checkpoint ACK 不清空记录。最近明细按旧到新排列，最多 20 条；更早明细与全局容量淘汰见 [保留范围](../OPERATIONS.md#上游拒答审计)。checkpoint 的 `state.refusals[user_id]` 保存相同结构，`state.events[].event.refusal` 保存本次报告，供管理端在 ACK 前归档完整历史。

LockLLM 不再调用，不需要其密钥；旧 `config.moderation` 仅兼容读取且导出时省略。原 `user.moderation` 是旧版历史，不再增长。新统计称为上游拒答，不能直接认定用户违规。

字段来源：[OpenAI Chat](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)、[OpenAI Responses SSE](https://developers.openai.com/api/reference/resources/responses/streaming-events)、[Claude 拒答格式](https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals)、[xAI Chat 兼容响应](https://docs.x.ai/developers/model-capabilities/legacy/chat-completions)。本地测试验证这些协议形状，不声称进行了真实安全违规触发测试。
