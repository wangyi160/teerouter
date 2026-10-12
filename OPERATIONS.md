# 运行与恢复

router：`teellm --admin-key-stdin [--bind 127.0.0.1:4000] [--tls-bind 0.0.0.0:8443]`。标准输入是一行管理凭证；无需数据库文件或数据库密钥。持久化由外部管理服务负责。

1. 冷启动 `GET /admin/checkpoint` 返回新的 boot_id、initialized=false。
2. 管理端发送 `POST /admin/initialize`：`{"boot_id":"当前值","state":已保存的state或null}`。null 只用于首次空初始化；已有服务必须恢复管理数据库保存的快照。
3. 初始化仅允许一次，验证全部配置、凭证、管理员身份与账号版本；失败不激活候选状态。拒答审计默认启用，无需下发独立审核服务的配置或密钥。
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

## 上游拒答审计

拒答审计默认启用，无需额外密钥。`/admin/status` 返回 `moderation_mode: "upstream_refusal"`、`lockllm_enabled: false`、`refusal_audit_enabled: true`。模型可用性仍受账号、目录、权限、限流和管理同步约束。缺少 LockLLM 配置不再阻止推理或目录查询；旧快照里的 `config.moderation` 仅在导入时兼容读取，新导出省略，既不会调用扫描服务，也不要求恢复它的密钥。

`GET /admin/users/{id}` 的 `user.refusals` 保存累计次数、最近拒答时间、已淘汰明细数和最近事件；`GET /admin/users` 也返回这些信息。原 `user.moderation` 只保留已有 LockLLM 历史，新拒答不混入旧违规统计。管理员查询结构见 [API 文档](docs/API.md#上游拒答与用户记录)。

请求结束、失败、超时或客户端取消时，每个请求最多累计一次拒答。流式未完成也保存已收到的信号和原文，并标记 `complete: false`，事件 `status` 说明 router 最终处理结果。拒答是上游响应，常见 HTTP 200 不会因审计被改成错误；明确安全错误码则返回 403 / `upstream_refusal`，记录中保留实际上游 HTTP 状态。检测到明确拒答后不切换账号或模型重试。

每用户保留最近 20 条明细（旧到新）；全局明细空间约 2 MiB，超限淘汰最旧明细并增加 `evicted_events`，累计次数不清零。每次记录最多 16 个拒答内容片段，拒答文本合计最多 8 KiB，Claude 随附可见文本、解释及安全错误说明分别最多 8 KiB；报告序列化后最多 16 KiB，超限标记 `truncated: true`。这些是审计存储限制，不截断转发给客户端的回复。

所有已观察的拒答也随调用事件进入 checkpoint / `/admin/logs`。外部管理服务必须先归档这些事件再 ACK，才能保留超过最近窗口的历史；ACK 只移除待同步事件，用户累计和最近记录继续保留。待同步事件达到 4096 条或约 2 MiB 后停止接收新请求，等待管理端同步；在途请求仍完成结算。恢复旧快照缺少 `refusals` 时按空记录处理。

此方式是在模型上游处理请求之后记录明确拒答，不执行输入预审核，不用文字关键词猜测拒答，也不保证发现所有不当请求。未标明 refusal 的普通自然语言拒答不会统计。上游类别和原因原样记录，不视作已证实的用户违规；用户的长上下文只发给所选模型上游，不再额外发送给 LockLLM。

本地回归使用受控 HTTP/SSE 服务，覆盖三家正式上游及 Workbuddy 的协议形状；不代表真实账号的拒答触发率验证。旧版本升级还需要更新镜像与客户端认可的 TEE 发布策略。router 仍为内存状态，断电可能丢失尚未同步的拒答事件。
