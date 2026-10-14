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

OAuth 凭证允许只提供 `access_token`，省略或清空 `refresh_token` 表示外部手动续期（Claude 始终由外部续期）。Router 不会对这些账号发起刷新请求，也不会因为进入提前刷新窗口而停用尚未过期的令牌。到期时间取 `expires_at`（Unix 秒）、Claude `expiresAt`（毫秒）或 JWT 的 `exp`；未提供且令牌不可解析时无法预告过期，只能在官方拒绝凭证后要求更新。

已过期账号在状态 API 中返回 `health.state: "expired"`、`health.last_error: "access_token_expired"` 和 `refresh_mode: "external"`，停止转发新请求；到期前被官方拒绝的凭证显示 `reauthorization_required`。重新导入完整的新凭证后恢复。提供有效刷新令牌的 OpenAI / xAI 账号仍自动续期，Workbuddy API key 行为不变。

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

`GET /admin/refusals?after_id=0&limit=100` 按 ID 升序读取所有用户的未归档拒答。记录包含 `user_id`、密钥名称和拒答详情；用户详情与列表不再提供 `refusals`。router 不维护按用户累计统计，完整历史与统计由管理端保存。原 `user.moderation` 仅兼容已有 LockLLM 历史，不再增长。接口与字段见 [API 文档](docs/API.md#上游拒答队列)。

管理端将一页记录持久保存后，执行 `DELETE /admin/refusals?through_id=N`，N 是已完整归档的最大连续前缀 ID，删除 `id <= N` 的全部记录。可以每次从队列头读取、保存、删除一页，也可用 `next_after_id` 继续分页；不要用尚未全部读完的 `last_id` 来确认删除。重复删除幂等，未来 ID 返回 409，新产生的较大 ID 不受影响。删除不会重置编号。

请求结束、失败、超时或客户端取消时，每个请求最多产生一条拒答记录。流式未完成也保存已收到的信号和原文，并标记 `complete: false`，事件 `status` 说明 router 最终处理结果。常见 HTTP 200 拒答不会因审计被改成错误；明确安全错误返回 403 / `upstream_refusal`，记录保留上游 HTTP 状态。确认拒答后不切换账号或模型重试。

队列积压达到 4096 条或约 2 MiB 时暂停新模型请求（503 / `accounting_unavailable`），已经在途的请求继续结算，因此可暂时超过阈值。队列不会自动淘汰，管理员仍可拉取和删除；删除后还须满足正常的用量同步条件才能恢复接收请求。`/admin/metrics` 的 `storage` 提供 `pending_refusals`、`pending_refusal_bytes`、`last_refusal_id`、`refusals_deleted_through`。分页默认 100 条，最大 500 条，不会序列化所有用户历史。

每条记录最多 16 个拒答片段，文本合计最多 8 KiB；Claude 可见文本、解释及安全错误说明分别最多 8 KiB，报告序列化后最多 16 KiB。触及限制时标记 `truncated: true`，不截断转发给客户端的回复。

checkpoint 的 `state.refusal_queue` 保存队列、`last_id` 与 `deleted_through`，用于停机恢复。checkpoint ACK 仅清理用量事件，不能确认拒答归档；反过来删除拒答也不删除用量。用量事件和 `/admin/logs` 仅保存 `refusal_id` 引用，拒答正文不会另存于用户信息或用量记录。用户删除、密钥轮换和配置热更新不会清除待归档拒答。

管理端以 `(instance_id, id)` 去重，并将归档与消费游标持久保存；删除后保存最新 checkpoint。正常恢复最新状态时 ID 继续递增。若只能恢复旧备份，管理端须结合已归档游标修正快照：将 `last_id` 至少推进到已归档最大 ID，按已确认前缀更新 `deleted_through` 并移除相应 events，避免重复编号或重新保留已归档记录。router 是内存服务，这不是独立持久化的恰好一次投递保证。

导入旧 `state.refusals` 时，将仍存在的用户窗口和旧待同步用量中的拒答报告按旧 ID 合并去重，迁移到统一队列并删除旧副本；首次迁移可能重放已归档旧历史，由管理员按游标删除。旧版已经淘汰且未备份的明细无法恢复；旧用户累计数字不迁移为新记录。缺少新旧拒答数据的快照初始化为空队列。

此方式是在模型上游处理请求之后记录明确拒答，不执行输入预审核，不用文字关键词猜测拒答，也不保证发现所有不当请求。未标明 refusal 的普通自然语言拒答不会统计。上游类别和原因原样记录，不视作已证实的用户违规；用户的长上下文只发给所选模型上游，不再额外发送给 LockLLM。

本地回归使用受控 HTTP/SSE 服务，覆盖三家正式上游及 Workbuddy 的协议形状；不代表真实账号的拒答触发率验证。旧版本升级还需要更新镜像与客户端认可的 TEE 发布策略。router 仍为内存状态，断电可能丢失尚未同步的拒答事件。
