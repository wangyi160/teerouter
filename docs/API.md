# Router API


路径相对于 router 地址；业务请求须先由独立客户端验证 TLS / TEE。

用户接口使用用户 API key；管理接口使用独立管理密钥。认证头为 `Authorization: Bearer <KEY>`，兼容 `x-api-key`，不接受 URL 中的 key。

`model` 使用目录中的提供商限定 ID，例如 `grok/grok-4.7`。使用 `X-Router-Routing` 指定账号池或自动匹配及倍率上限；旧客户端也可用 `x-router-pool: m_<id>.pool` 选择账号池。思考参数取目录允许值，省略时不补写。金额和账务属于管理平台。

## 用户调用

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/v1/models` | 按当前 API key 的权限返回已发现模型及思考能力；模型目录与账号池分别读取。 |
| GET | `/v1/pools` | 返回当前 API key 可访问的池、供应商类型和账号运行状态；不返回账号凭证。 |
| GET | `/v1/usage` | 当前 API key 的调用和 token 统计；金额与历史账单由管理平台计算。 |
| POST | `/v1/chat/completions` | messages 请求格式，可使用 stream；思考强度字段为 reasoning_effort。 |
| POST | `/v1/responses` | input 请求格式，可使用 stream；思考强度字段为 reasoning.effort。不支持 background=true。 |
| POST | `/v1/messages` | messages 请求格式，需 max_tokens；thinking 选项以模型目录为准。 |

## 请求路由规则 v1

选路规则是 router 协议的一部分，适用于三个生成接口。客户端通过单个 `X-Router-Routing` 请求头发送 JSON（最多 2048 字节），不改写模型请求正文。Router 在 TEE 内完成选池、倍率检查及准入，规则头不会发给供应商。

固定池：

```http
X-Router-Routing: {"version":1,"mode":"fixed","pool":"m_<id>.pool","max_multiplier_millionths":500000,"priority":"price"}
```

自动匹配：

```http
X-Router-Routing: {"version":1,"mode":"auto","max_multiplier_millionths":500000,"priority":"speed"}
```

| 字段 | 语义 |
| --- | --- |
| `version` | 必填整数，当前为 `1` |
| `mode` | 必填，`fixed` 或 `auto` |
| `pool` | 固定模式必填完整商户池 ID；自动模式不允许填写 |
| `max_multiplier_millionths` | 必填非负整数，最大 10^15；500000 表示最高 0.5×，等于上限允许 |
| `priority` | `price` 或 `speed`，默认 `price`；固定模式不比较其他池 |

规则与 `X-Router-Pool`、`X-Router-Account`、`X-Router-Price-Version`、`X-Router-Official-Price-Revision` 互斥。重复规则头、未知字段、未知版本和无效类型返回 400。`X-Router-Session` 仍可用于会话绑定。不带规则的旧请求继续按原有商户/池选择方式处理。

客户端须在同一条已验证的 TEE/TLS 连接读取 `/health/live`，确认 `routing_protocol_versions` 包含 `1` 后再发送生成请求。旧 router 可能忽略未知请求头，不能依赖生成后的响应来判定是否支持。

Router 按用户和商户授权、精确模型 ID、接口与官方能力、账号状态、并发及速率限额筛选。选池与倍率校验使用同一份配置、报价快照，该报价随用量归档；之后调价不改变已准入请求的价格。固定池超限返回 `403 routing_multiplier_exceeded`，不会换池；自动模式无匹配池返回 `503 routing_no_matching_pool`，不会提高上限或更换模型。带商户前缀的模型仍限制在指定商户内。

`price` 选择最低倍率。`speed` 使用 router 观测的真实流式生成内容：首字时间优先，同一 100 ms 区间再比较 token/s，再按倍率和池 ID 决定。样本按池、模型和接口区分，移动平均仅保存在 router 内存中，10 分钟后过期；无样本时从正常请求轮流积累，不发送探测请求。心跳及角色元数据不计首字，短于 250 ms 的突发不估算生成速度；非流式响应、错误、截断及取消不伪造成功速度。测速在流式转发内部进行，即使客户端不要求返回 usage，也可利用上游 token 统计。不保存请求或回复正文，重启后重新积累。

已绑定的会话和响应续接留在原商户、原账号。自动模式的未知 `previous_response_id`、无已知归属的 provider 文件或跨商户归属歧义返回 `409 routing_affinity_required`，要求指定原账号池，不能以价格或速度猜测上下文归属。

成功响应包含 `X-Router-Routing-Version: 1`、`X-Router-Pool`、`X-Router-Multiplier-Millionths`、`X-Router-Price-Version`，以及原有商户、deployment 和 attempts 头。规则不授予额外权限，实际调用仍须通过平台和商户准入。并发变化可能导致最终准入拒绝，不会为此放宽上限或越过授权。

## 健康检查与 TEE 验证

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/health/live` | 200 表示进程可响应，不代表可以调用模型；`routing_protocol_versions: [1]` 声明路由规则版本。 |
| GET | `/health/ready` | 可接收业务时返回 200，初始化、同步或上游不可用时返回 503。 |
| POST | `/.well-known/teellm/attestation` | 仅在 TLS 入口提供。nonce 为客户端新生成的 32 字节随机数的小写十六进制编码（64 位）；客户端验证硬件签名、发布策略和当前 TLS 证书绑定。 |

## 管理接口

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/admin/status` | 就绪状态、内存存储、初始化状态、boot_id、在途请求和账号健康情况。 |
| GET | `/admin/metrics` | 入口并发、存储、账号、发现与自动维护指标。 |
| GET | `/admin/usage` | 读取用量统计，不包含用户的问题和回答。 |
| GET | `/admin/config` | 返回脱敏配置及 revision。 |
| PATCH | `/admin/config` | 带 revision 提交配置补丁；过期 revision 被拒绝。 |
| POST | `/admin/apply` | 下发平台用户、key 和资源配置；拒绝平台持有上游账号池、账号及凭证。 |
| GET | `/admin/users` | 用户、所属 API key、并发权限及旧 moderation 历史；不附带拒答队列。 |
| POST | `/admin/users` | 创建用户及其访问权限；金额不属于 router 用户属性。 |
| GET | `/admin/users/{id}` | 读取指定用户及其 key、权限、旧 moderation 历史。 |
| PUT | `/admin/users/{id}` | 更新指定用户及其权限。 |
| DELETE | `/admin/users/{id}` | 撤销指定用户及其 API key。 |
| GET | `/admin/checkpoint` | 管理后端用于加密保存配置、凭证、待确认用量与拒答队列及其游标；响应含敏感数据。 |
| POST | `/admin/checkpoint/ack` | 持久保存后确认用量消费游标，并续期服务租约；不删除拒答队列。 |
| POST | `/admin/initialize` | 绑定本次 boot_id 恢复管理平台保存的快照。 |
| GET | `/admin/logs` | 待同步调用元数据，拒答仅含 refusal_id 引用；不含拒答正文。 |
| GET | `/admin/refusals` | 按 after_id、limit 分页拉取所有用户的待归档拒答。 |
| DELETE | `/admin/refusals` | 按 through_id（含该 ID）批量删除已归档前缀。 |
| POST | `/admin/maintenance` | 触发内部维护任务；正常使用由 router 自动执行。 |

## 上游拒答队列

账号、模型发现和销售费率只通过 [商户管理接口](MERCHANTS.md) 操作。`/admin/accounts*`、`/admin/models*` 返回 404；未提供路由规则且无商户标识的模型调用返回 `merchant_required`。

管理端统一拉取所有用户的待归档记录，持久保存后显式删除。接口要求管理 Bearer 凭证并返回 `Cache-Control: no-store`。用户 API key 无法读取或删除；用户接口不再返回 `refusals`。

`GET /admin/refusals?after_id=0&limit=100` 返回严格大于 `after_id` 的记录，按全局 ID 升序排列。`after_id` 默认 0，`limit` 默认 100、允许 1–500。查询不消费记录，未归档记录不会按数量或时间自动淘汰。

```json
{
  "instance_id": "persistent-router-instance-id",
  "last_id": 12,
  "deleted_through": 11,
  "pending": 1,
  "next_after_id": 12,
  "has_more": false,
  "data": [{
    "id": 12,
    "user_id": "user-1",
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

`next_after_id` 是本页最后一条 ID；空页时为查询游标与已删除水位的较大值。`pending` 是整个队列剩余数量，`last_id` 是已发出的最大编号，不能在未保存前面全部记录时用它删除。每条包含 `user_id`，无需遍历用户；`key` 是密钥名称，不是密钥值。编号独立于用量 seq，删除不重置，恢复最新 checkpoint 后继续递增；迁移旧版可能出现 ID 间隙。`instance_id` 在正常恢复时保持，管理端用 `(instance_id, id)` 去重。

`DELETE /admin/refusals?through_id=12` 不需要请求正文，删除全部 `id <= 12` 的记录：

```json
{"instance_id":"persistent-router-instance-id","deleted":1,"deleted_through":12,"last_id":12,"pending":0}
```

管理端必须先归档再删除；重复或更小的已确认 ID 返回成功、`deleted: 0`。大于 `last_id` 的读取或删除游标返回 409 / `invalid_refusal_cursor`；缺少删除 ID、负数、未知参数或不合法 limit 返回 400。新产生的更大 ID 保留。删除实际清理队列正文，用户资料和用量日志不保留其副本；用量中的 `refusal_id` 仅用于关联。

推荐流程：读取一页 → 事务保存该页及游标 → DELETE 到本页最后 ID → 再读一页。checkpoint 同步与此流程独立：`state.refusal_queue` 保存未删除队列、编号和删除水位，ACK 仅消费用量，不能替代 DELETE。删除后同步最新快照；恢复旧备份时的游标修正、队列满时暂停服务的行为见 [运行文档](../OPERATIONS.md#上游拒答审计)。

`protocol` 和 `streamed` 描述实际上游协议；Codex 即使客户端请求 JSON，也可能返回 SSE。`complete` 表示读到完整上游响应；`status` 是 router 最终结算状态，例如流中断 502、超时 504、客户端取消 499，`upstream_status` 是上游 HTTP 状态。缺少的元数据为 null。存储触及限制时 `truncated: true`，不会因此截断客户端收到的回复。

| 上游 / 接口 | 明确信号 | 保存内容 |
| --- | --- | --- |
| OpenAI / Grok / Workbuddy Chat | `choices[].message.refusal`、`delta.refusal`、`finish_reason: content_filter` | 合并的拒答原文、命中字段、结束原因、响应标识 |
| OpenAI / Grok Responses（含 Codex 转换） | `type: refusal` 内容块、`response.refusal.delta/done`、`incomplete_details.reason: content_filter` | 按 output/content 索引合并的原文和信号；重复 done/最终快照不重复累计 |
| Claude Messages（含 Chat 转换） | `stop_reason: refusal`、`stop_details.type: refusal` | `stop_details.type/category/explanation` 与同一响应的可见文本；不保存思考内容 |
| 上游明确安全错误 | `error.code/type` 为 `content_policy_violation`、`content_filter`、`safety_violation` | 错误 code/type/message 和原 HTTP 状态；单独的 400/403/429 不算拒答 |

普通 HTTP 200 拒答沿原有协议返回；明确安全错误返回 403 / `upstream_refusal`，不会重试其他账号。流已经开始后的传输错误仍走 SSE `stream_interrupted` 路径，同时保留已观察的拒答。一般的拒答文本、logprobs、工具输出或用户输入不用于推断拒答。Grok/Workbuddy 只在实际返回上述字段时记录，不能保证每次拒答都有结构化标志。

每个请求完成或中断时最多入队一次。用户/密钥更新、用户删除和配置热更新不删除待归档记录；按用户累计及完整历史由管理端维护。

LockLLM 不再调用，不需要其密钥；旧 `config.moderation` 仅兼容读取且导出时省略。原 `user.moderation` 是旧版历史，不再增长。新统计称为上游拒答，不能直接认定用户违规。

字段来源：[OpenAI Chat](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)、[OpenAI Responses SSE](https://developers.openai.com/api/reference/resources/responses/streaming-events)、[Claude 拒答格式](https://platform.claude.com/docs/en/test-and-evaluate/strengthen-guardrails/handle-streaming-refusals)、[xAI Chat 兼容响应](https://docs.x.ai/developers/model-capabilities/legacy/chat-completions)。本地测试验证这些协议形状，不声称进行了真实安全违规触发测试。
