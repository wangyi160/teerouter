# 运行与恢复

router：`teellm --admin-key-stdin [--bind 127.0.0.1:4000] [--tls-bind 0.0.0.0:8443]`。标准输入是一行管理凭证；无需数据库文件或数据库密钥。持久化由外部管理服务负责。

1. 冷启动 `GET /admin/checkpoint` 返回新的 boot_id、initialized=false。
2. 管理端发送 `POST /admin/initialize`：`{"boot_id":"当前值","state":已保存的state或null}`。null 只用于首次空初始化；已有服务必须恢复管理数据库保存的快照。
3. 初始化仅允许一次，验证全部配置、凭证、管理员身份与账号版本；失败不激活候选状态。恢复 state 时由管理员在 `state.bundle.config.moderation` 和 `state.bundle.secrets` 下发审核配置及密钥；首次空初始化后通过 `/admin/apply` 下发。缺少审核配置时可继续管理和同步，但拒绝模型请求。
4. 管理端读取 `/admin/checkpoint`，原子保存 state、用量和计费结果，然后 POST `/admin/checkpoint/ack`，包含同一 boot_id、generation、sequence。
5. ACK 只清除已同步用量，不清除读取快照之后产生的新用量。每轮同步重复此过程。密码、token 和完整快照必须经经过验证的保密连接传输，不放在日志中。

管理 API 沿用 Bearer 认证，快照响应禁止缓存；业务 API key 不能读取快照或初始化。账务 API 仅存在于独立管理平台，router 不接受充值、余额或定价命令。

每轮同步后等待 1 秒。管理确认超过 30 秒未更新时暂停新业务请求；已经开始的请求继续完成。达到待同步 usage 上限也暂停新请求，避免长期断联占满内存。模型目录查询同样要求管理连接健康。

计划重启：暂停新请求，等待在途请求结束，通过 `/admin/checkpoint` 保存最新状态并 ACK 后停止 router。新进程等待外部管理服务恢复；不要用首次空初始化覆盖已有状态。

注意：周期快照有崩溃丢失窗口，包括用量和轮换 refresh token。需要更强保证时必须引入同步持久化协议，当前精简版本没有承诺零丢失。


## 供应商范围与升级

当前只接受 `openai`、`anthropic`（别名 `claude`）、`xai`（别名 `grok`），以及启用 `workbuddy-lab` 构建时的 `workbuddy`。Gemini / Gemini CLI 已移除，旧的 `/v1beta/models/*` 接口返回 404。

升级前应由管理服务保存旧版加密快照，移除 Gemini 账号、部署、空池及其关联的池权限、fallback、restrictions、compatibility 和专用密钥，再保存并恢复新快照。禁用但仍存在的 Gemini 账号或部署也会被拒绝；不要用空初始化覆盖用户及用量。其余提供商的旧快照允许 `gemini_project: null` 和 `client_secret_ref: null`，新版导出时省略；非空值会被拒绝。管理端的供应商选项也应同步移除 Gemini。

## 用户与并发 API

所有 `/admin/users` 接口要求管理 Bearer 凭证；用户资料不接受余额、额度或价格字段。完整 key 仅在认证后的管理读取接口显示，响应禁止缓存。

| 方法 | 路径 | 行为 |
| --- | --- | --- |
| GET | `/admin/users` | 所有用户，包含各自 keys 和 in_flight |
| POST | `/admin/users` | 创建用户，可原子导入全部 keys |
| GET | `/admin/users/{id}` | 单个用户及其 keys |
| PUT | `/admin/users/{id}` | 替换用户资料，省略 keys 时保留现有 keys |
| DELETE | `/admin/users/{id}` | 撤销用户、其全部 keys 和不再引用的 key 密钥 |

例如创建用户（密钥值为示例）：

```json
{"id":"user-1","name":"示例用户","note":"","enabled":true,"max_concurrent":4,
 "keys":[{"name":"key-1","key":"replace-with-random-api-key","models":[]}]}
```

`keys` 中的 `user_id` 自动绑定当前用户，不允许指定其他用户。提供 `keys: []` 会撤销该用户全部 key；修改列表中的 key 值可轮换密钥。POST/PUT 可带 `expected_revision`，版本冲突返回 409；非法资料或缺少密钥引用时整次更新失败。批量下发用 `/admin/apply` 的 `config.users` 和 `config.keys`，所有 key 必须有合法的 `user_id`。

用户并发范围为正在执行的模型请求，所有关联 API key 共享 `max_concurrent`（1 至 100000，默认 16）。超限响应为 429 / `user_concurrency_limit`，不会排队。流式调用直到流结束或客户端断开才释放。单个 key、router 总体和上游账号的限制仍可能更严格。

余额、已用金额、额度、价格和结算由外部管理服务负责，不是 router 用户属性。

## LockLLM 输入审核

LockLLM 是模型服务的必需配置。管理员在初始化/恢复状态时下发 `config.moderation` 与对应 secrets；首次空初始化后，以管理密钥调用 `POST /admin/apply`，提交 [moderation-update.example.json](moderation-update.example.json)。将示例值换成真实 LockLLM 密钥，可以加 `expected_revision` 做并发控制。密钥只放入 secrets，通过 api_key_ref 引用。不要把密钥写进客户端请求或提交到仓库。

缺少 `config.moderation`（省略或 null）时，全部模型推理接口（含流式）和 `/v1/models` 返回 503 / `moderation_not_configured`，不发起模型推理；`/health/ready` 返回 503，`/admin/status` 的 `moderation_configured` 为 false。存活检查、管理接口和用量查询仍可使用。该拒绝发生在模型请求计费和扫描之前，不记用户违规或扫描次数。引用不存在或空密钥的初始化/配置更新会原子失败，继续使用原状态；有效配置必须同时包含独立审核密钥。恢复不含审核配置的旧快照也不会开放模型服务。

| 配置项 | 默认值 | 含义 |
| --- | --- | --- |
| api_key_ref | 必填 | secrets 内的独立 LockLLM 密钥名称 |
| sensitivity | medium | low / medium / high |
| timeout_secs | 30 | 审核总时限 1–300 秒，也受 router 整体请求时限约束 |
| max_input_bytes | 16777216 | 提取后完整文本的 UTF-8 字节上限，不是 token 上限；超限拒绝，不截断 |
| max_concurrent | 16 | 所有用户共享的审核并发上限 |
| rpm | 300 | 所有用户共享的每分钟扫描调用上限，按账号实际额度配置 |
| proxy | null | 可选 http / https / socks5h 显式代理；省略时直连，仅针对审核服务 |

接口固定为 `https://api.lockllm.com/v1/scan`，不接受管理员自定义域名或路径，禁用重定向和系统代理继承。审核始终使用 combined、scan-action=block、policy-action=block、on-unavailable=block；客户端不能覆盖。整个当前请求中的可见文本、角色、顺序、指令、工具定义/调用/结果一起序列化成 input，全量提交，由服务自动分块；不做截断、摘要、逐轮增量或会话缓存。模型请求本身不因审核而改写。

审核不向服务发送 router 用户密钥、上游模型密钥、客户端认证头或管理凭证，也不发送外层 metadata/user 标识字段。当前扫描的是文本，图片、音频、附件、加密推理块、previous_response_id/conversation/cachedContent 或服务端 prompt 模板等不可见内容引用会明确报不支持；不要将其当作已经完整审核。通过后直接调用原上游，本次仅做输入审核，模型回复和 SSE 内容不经过 LockLLM。

使用 `{"config":{"moderation":null}}` 会停止接受新的模型请求，不能绕过审核；已接受的请求沿用其配置代次完成，历史统计继续保留。配置热更新、移除后重新下发或更换密钥都不会重置本进程的审核并发和 RPM 计数。用户统计及最近事件存在内存并随 checkpoint 导出，管理端恢复旧快照时缺少该字段视为零；重启时仍必须恢复已保存的 state。管理员查询和错误码见 [API 文档](docs/API.md#输入审核错误与用户记录)。

上线前需要真实账号联调。LockLLM 的 [API 文档](https://www.lockllm.com/docs/api-reference#behaviour-when-a-check-is-unavailable) 说明故障拒绝能力仍按账号逐步开放：通过结果必须带 `X-LockLLM-On-Unavailable: block`，否则本实现返回 503。也会拒绝 degraded、缺失或零置信度等不完整结果。文档目前的 detection_status 只明确覆盖威胁检测，不能把 policy_confidence 的存在当作外部内容政策检查一定成功的证明；供应商内容政策故障的实际行为仍须验证。此集成不保证分类零漏判，也不能检测服务端未报告的故障。

首次部署应验证中文与代码误判、全量长上下文的延迟/大小上限、余额不足、429、超时和故障拒绝。自动分块参考 [官方长文本指南](https://www.lockllm.com/docs/api-reference#scanning-long-documents)。本地测试使用受控服务，不消耗真实 API 额度。审核费用属于外部管理账务，不能计作模型 token 用量。
