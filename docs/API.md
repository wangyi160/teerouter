# Router API

消费流水签名协议见 [CONSUMPTION-PROOFS.md](CONSUMPTION-PROOFS.md)：
`POST /admin/checkpoint/diff` 可设置 `sign_receipts:true`，返回独立签名批次；
`POST /admin/checkpoint/ack` 同时提交 `receipt_batch_hash`；
`POST /admin/receipts/evidence` 用新随机 nonce 获取消费签名公钥的硬件证明。


路径相对于 router 地址；业务请求须先由独立客户端验证 TLS / TEE。

用户接口使用用户 API key；管理接口使用独立管理密钥。认证头为 `Authorization: Bearer <KEY>`，兼容 `x-api-key`，不接受 URL 中的 key。

`model` 使用目录中的提供商限定 ID，例如 `grok/grok-4.7`。使用 `X-Router-Routing` 指定账号池或自动匹配及倍率上限；旧客户端也可用 `x-router-pool: m_<id>.pool` 选择账号池。思考参数取目录允许值，省略时不补写。逐请求费用由 Router 计算，余额和持久化账务属于管理平台。

## 用户调用

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/v1/models` | 按当前 API key 的权限返回已发现模型及思考能力；模型目录与账号池分别读取。 |
| GET | `/v1/pricing` | 已登录用户读取完整官方价格簿，独立于账号池、账号和可调用模型权限；不含商户倍率或 Workbuddy 测试价。 |
| GET | `/v1/pools` | 返回当前 API key 可访问的池、供应商类型和账号运行状态；不返回账号凭证。 |
| GET | `/v1/usage` | 当前 API key 的调用和 token 统计；逐请求金额由 Router 计算，历史账单和余额由管理平台保存。 |
| POST | `/v1/chat/completions` | messages 请求格式，可使用 stream；思考强度字段为 reasoning_effort。 |
| POST | `/v1/responses` | input 请求格式，可使用 stream；思考强度字段为 reasoning.effort。不支持 background=true。 |
| POST | `/v1/messages` | messages 请求格式，需 max_tokens；thinking 选项以模型目录为准。 |

## Agent 与模型提供商匹配 v1

三个模型生成接口都执行同一固定规则：Codex 只能使用 OpenAI（模型目录前缀 `chatgpt/`），Claude Code 只能使用 Claude（`claude/`），Grok Build 只能使用 xAI（`grok/`）。其他客户端不得调用正式模型服务。显式启用的 Workbuddy 实验池保留测试例外，可接受其他客户端；它不能作为正式服务的备用路由。

Router 识别原始 `User-Agent` 的产品标记，并校验随附的 `originator`、`x-app`、`x-grok-client-identifier` 是否一致。重复、合并或矛盾的身份头返回 `400 invalid_agent_identity`；不支持的客户端返回 `403 unsupported_agent`；已识别客户端与模型提供商不符返回 `403 agent_provider_mismatch`。这些拒绝发生在模型转发和计费准入之前。

约束同时用于提供商限定模型、未限定模型、固定池、自动选池、旧版池选择头以及失败备用路由。以实际上游提供商为准，不能通过相同模型名、模型别名或低费率绕过。原始客户端标记由 teeproxy 透传，Router 不会根据模型名补造客户端身份。`GET /v1/models`、价格和账号池等查询接口不执行客户端类别限制，返回目录仍取决于 API key 权限。`/admin/status` 返回 `agent_provider_policy.version: 1`。

这是兼容性策略，并非客户端软件的远程证明。HTTP 请求头可以伪造，因此不能据此证明请求确实来自某个未修改的官方程序，也不能保证上游模型质量。

实现依据：Codex 官方实现的 [默认 HTTP 客户端](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth/default_client.rs)，Claude Code 官方 npm 包中的 `claude-cli/<version>`、`x-app: cli`（`@anthropic-ai/claude-code` 2.1.0），Grok Build 官方实现的 [sampling client](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-sampler/src/client.rs)。CLI、Codex IDE、Grok pager 等已知标记均有回归用例；客户端更改这些标记后需要更新识别规则，不自动放行未知身份。

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

`price` 比较预计总 token 费用：普通输入、缓存读取/写入、输出价格与池倍率，同时应用报价中的长上下文及服务等级规则。输入以序列化字节数 / 3 估算，输出采用请求的输出上限（未指定时为 1024）；该结果仅用于路由，实际账单始终按上游用量结算。相同倍率不代表相同实际费用，已缓存账号可能优于倍率更低的冷账号。

缓存估算只使用同一用户、模型、接口、会话和上游账号的成功响应记录。连续前缀摘要必须匹配；缓存记录最多按 5 分钟有效，仅把可复用且已观测输入的一半计作预计缓存读取，避免把提供商可能驱逐的缓存当成必然命中。Claude 还要求已有缓存读取/写入用量，不能把普通输入视作已缓存。重复请求报告未命中时撤销折扣预期；没有记录、记录过期或修改前缀均按冷输入估算。Claude 未指定缓存控制时启用官方 5 分钟自动缓存，已有断点保持不变；写入费用计入估算和实际账单。

普通请求自动获得软会话粘性：优先识别 `session_id`、`x-session-id`、`x-conversation-id`、`x-grok-conv-id` 和 `prompt_cache_key`；未提供时使用初始对话前缀摘要推断。续接优先使用原池、原账号；`price` 在其他池包含缓存后的估价更低时可以切换。`speed` 对可用的原池优先复用，新会话或原路由不可用时才按速度择池。始终重新校验授权、倍率上限、模型范围和账号容量。无明确标识时，具有相同初始前缀的会话可能共用路由偏好，压缩或改写初始上下文可能被识别为新会话；仅影响路由，不合并对话内容。

粘性记录仅在内存中保存摘要与计数，每个商户最多 4096 条账号记录，平台最多 4096 条会话选池记录，24 小时不使用即不再复用；普通配置更新保留，替换账号凭证或代理时清除对应记录。OpenAI/Grok 会收到稳定的缓存路由标识；Chat → Responses 转换及 OAuth 路径也保留该标识。客户端无需自己选择账号，teeproxy 继续透传标识和请求。

`speed` 表示**低延迟优先**，保留原协议字段以兼容已保存的规则。Router 每 5 分钟通过账号实际配置的 HTTP、SOCKS5h 或 VLESS 代理，对官方 API 入口的模型目录发起无凭证 HTTPS HEAD 请求，不调用模型生成接口、不刷新令牌、不下载响应正文。相同代理配置和入口共用样本；探测使用新连接，耗时包含代理隧道、TLS 和 HTTP 响应头等待，不能代表模型排队或生成速度。401、404、405 可作为连通证据，但不证明账号授权有效；403、429、重定向和服务端错误不作为成功延迟。

后台最多同时探测 8 条链路，单次最多 10 秒，拥塞时分批处理。启动、解锁和配置变更后后台发现新入口；请求选路只读取内存结果，不等待探测。成功延迟使用旧值 70% / 新值 30% 的移动平均，10 分钟后过期，重启后重新积累。每个池按可用账号各自的链路结果选择，优先保留没有探测失败的原会话账号，再比较延迟；健康候选优于近期探测失败的候选，失败不单独停用账号。当所有候选都无有效样本时，按粘性、包含缓存的预计费用和原账号调度规则兜底；不会为学习速度刻意轮流发送模型请求。倍率、授权及硬会话归属始终有效。`/health/live` 返回 `speed_routing: "proxy_https_latency"` 和 `proxy_probe_interval_secs: 300`；管理员可在 `/admin/metrics` 的 `proxy_probes` 查看探测数量，无代理地址或凭证。

`/v1/pools` 的池及 `accounts[]` 各自包含 `proxy_latency`，例如 `{"status":"measured","latency_ms":170.25,"checked_at":1792000000,"expires_at":1792000600}`。时间为 Unix 秒。状态为 `measured`、`unknown`（未测速）、`failed`、`stale` 或 `unavailable`（账号不可用，或池没有可用账号）；非成功状态的延迟为 `null`。池级结果取可用且有可见模型账号中的最低有效延迟，不代表每次选中的粘性账号或模型首字、生成速度。`/admin/public-catalog` 提供同样的池级结果，不含账号和代理信息。读取目录不会触发测速；客户端缓存到 `expires_at` 后必须显示过期。

原有真实请求的首字时间和 token/s 统计仅保留为内存诊断，不再参与低延迟排序，不保存请求或回复正文。

显式 `X-Router-Session` 及响应续接仍为硬绑定，留在原商户、原账号。自动模式的未知 `previous_response_id`、无已知归属的 provider 文件或跨商户归属歧义返回 `409 routing_affinity_required`，要求指定原账号池，不能以价格或速度猜测上下文归属。

成功响应包含 `X-Router-Routing-Version: 1`、`X-Router-Pool`、`X-Router-Multiplier-Millionths`、`X-Router-Price-Version`，以及原有商户、deployment 和 attempts 头。规则不授予额外权限，实际调用仍须通过平台和商户准入。并发变化可能导致最终准入拒绝，不会为此放宽上限或越过授权。

## 健康检查与 TEE 验证

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/health/live` | 200 表示进程可响应，不代表可以调用模型；`routing_protocol_versions: [1]` 声明路由规则版本。 |
| GET | `/health/ready` | 可接收业务时返回 200，尚未初始化、主动停机或上游不可用时返回 503；管理断联本身不影响就绪状态。 |
| POST | `/.well-known/teellm/attestation` | 仅在 TLS 入口提供。nonce 为客户端新生成的 32 字节随机数的小写十六进制编码（64 位）；客户端验证硬件签名、发布策略和当前 TLS 证书绑定。 |

## 管理接口

| 方法 | 路径 | 用途 |
| --- | --- | --- |
| GET | `/admin/status` | 就绪状态、内存存储、初始化状态、boot_id、在途请求和账号健康情况。 |
| GET | `/admin/metrics` | 入口并发、存储、账号、发现与自动维护指标；`lifecycle` 包含 `quiesce_version: 1`、`quiescing`、`background_jobs` 和 `drained`。`/admin/status` 同样提供 `lifecycle`。 |
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
| POST | `/admin/checkpoint/diff` | 提交上次 boot_id、base_id，返回 checkpoint_id 和变化字段；首次请求或基线失效返回完整 state。仅平台管理员可用，停机排空期间仍可读取。 |
| POST | `/admin/checkpoint/ack` | 持久化结算后提交 boot_id、generation、sequence 和 balances_patch（变化的用户余额；null 删除）或完整 balances，二者互斥。原子更新余额、清除已同步消费，不删除拒答队列，不承担服务保活。 |
| POST | `/admin/initialize` | 绑定本次 boot_id 恢复管理平台保存的快照。 |
| POST | `/admin/quiesce` | 管理员提交 `{"boot_id":"当前进程 ID","quiescing":true}` 暂停新业务和后台任务；轮询 metrics 的 `lifecycle.drained`，排空后保存快照再停机。提交 false 恢复服务；重启后须重新初始化。 |
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
# 自动价格数据源

平台管理接口：`GET/PUT /admin/pricing/source`、`POST /admin/pricing/refresh`。详见 [配置、状态及导入规则](PRICE_SOURCE.md)。能力标识为 `/admin/status` 的 `pricing_source_sync_version: 1`。
