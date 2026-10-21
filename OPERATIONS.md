# 运行与恢复

## 错误、中断与估算计费（billing v2）

请求状态与计费状态独立。HTTP 错误、流中错误、超时或客户端断开，都不能抹去已经获取的官方用量。每次上游尝试独立采集、使用请求开始时冻结的价格；各次尝试的未舍入金额相加后乘账号池倍率，最后一次向上取整到 micro-USD。不要再按 HTTP 成功与否过滤 v2 账单，旧 v1 记录的规则保持不变，不追溯补扣。

只有实际发给模型的请求发生网络错误、上游错误或中断，并且缺少完整官方用量时，才允许根据已观察到的生成文本或正数官方计量数据估算。HTTP 200 内的明确上游错误同样属于失败；正常成功返回（包括正常结束的流）即使没有 usage，也只标记待核算，不估算扣费。模型列表、健康检查、管理请求不参与模型估算，只有请求正文或普通错误消息也不构成生成证据。

`o200k_json_text_v1` 对请求中的 messages/input/instructions/system/tools 的 JSON 表示和已观察到的输出文本使用固定的 o200k_base 分词器；它是三个提供商共用的估算方法，不宣称与 Claude、Grok 或任何模型的真实分词完全一致。分词仅在满足上述失败条件时执行，以固定 4096 字节 UTF-8 安全窗口限制计算开销；正常请求不计算本地估算。流式文本先按内容块拼接，终态快照替换相应内容块，避免按网络分块重复计数。官方已确认的输入与缓存数量优先；输出使用已报告累计数量与可见输出估算的较大值。若失败请求在结算前已收到完整官方用量，直接按官方用量结算，不产生估算费用；不存在先对每笔请求估算扣费、再等待官方替换的流程。

没有缓存数据的估算输入按普通输入价格收费，可能高于实际缓存折扣后的费用。无法观察的隐藏推理、未传回的输出和服务端历史上下文不推算；`previous_response_id`/conversation 隐含的上下文会标记不完整。多模态输入不按 base64 或 URL 文本猜算；估算文本超过 2 MiB、内容块超过 1024、价格维度缺失或官方数字不一致时仍待核算。估算只保存数字和算法标识，请求/回复正文不进入账单或公开流水。

生成前的限流、鉴权/参数拒绝且无用量时为零费用；纯连接失败不按提交的请求文本扣费。尚未看到任何生成或计量证据的超时保留未知状态。`attempt_records` 保留各次尝试的状态、用量、计价项和估算依据；一个尝试未知不会抹掉其他已确认或可估算尝试的费用。`usage_source` 为 official/estimated/mixed/none，`coverage` 为 complete/partial/unknown，`unknown_attempts` 表示未能核算的尝试数；calculated 只说明记录中的金额已计算，并不表示用量一定完整。用户及商户账单必须展示估算或部分用量标识。

`GET /v1/pricing` 的 `billing_policy` 公布上述规则。管理端和公开流水接收服务必须先支持 v2，再启用新版 Router；旧接收端应拒绝不理解的账单，不能默默按旧公式补算。估算金额直接进入内存消费、签名批次和管理端入账，沿用原有幂等与结算确认规则。

## 运行与恢复流程

router：`teellm --admin-key-stdin [--bind 127.0.0.1:4000] [--tls-bind 0.0.0.0:8443] [--journal-dir PATH]`。标准输入是一行管理凭证。可选的本地流水日志使用 dstack TEE 派生密钥；不接受管理员提供的日志密钥。配置、账号与最终账务仍由独立管理服务持久化，流水恢复规则见 [本地流水日志](docs/ACCOUNTING-JOURNAL.md)。

1. 冷启动 `GET /admin/checkpoint` 返回新的 boot_id、initialized=false。
2. 管理端发送 `POST /admin/initialize`：`{"boot_id":"当前值","state":已保存的state或null}`。null 只用于首次空初始化；已有服务必须恢复管理数据库保存的快照。
3. 初始化仅允许一次，验证全部配置、凭证、管理员身份与账号版本；失败不激活候选状态。拒答审计默认启用，无需下发独立审核服务的配置或密钥。
4. 管理端通过 `/admin/checkpoint/diff` 获取增量，在本地重建完整快照，在同一事务中保存快照、结算新消费、更新用户余额和消费游标，然后 POST `/admin/checkpoint/ack`，包含同一 boot_id、generation、sequence 及 `balances_patch`（变化的用户 ID → 整数 micro-USD，删除使用 null）。旧客户端仍可读取完整 `/admin/checkpoint` 并提交完整 `balances`。
5. ACK 原子更新余额并清除已同步消费；读取快照之后产生的新消费保留到下一轮。管理端的恢复快照使用结算后的余额、空 events 和空 consumption，重启时不会重复扣款。密码、token 和完整快照必须经经过验证的保密连接传输，不放在日志中。

管理 API 沿用 Bearer 认证，快照响应禁止缓存；业务 API key 不能读取快照或初始化。充值和持久化账务由独立管理平台负责；Router 通过同步 ACK 接收余额，通过价格 API 管理报价。

管理平台每轮自动同步后等待 300 秒；管理员保存修改、充值或点击同步时立即下发。初始化完成后，Router 独立处理业务请求、模型目录查询、账号令牌维护及价格更新；管理平台断联不会触发停服。断联期间继续使用上次同步余额减去未同步消费检查余款，消费等待管理平台恢复连接后结算。达到待同步 usage 上限仍暂停新模型请求，避免占满内存。`storage.manager_connected` 仅表示最近 630 秒收到过 ACK（容纳两次定时同步和网络延迟），不参与请求准入。

增量协议：`POST /admin/checkpoint/diff` 请求体为 `{"boot_id":"上次启动标识","base_id":"上次 checkpoint_id"}`，首次均可为 null。响应携带 `checkpoint_diff_version: 1` 和新的 `checkpoint_id`；有效基线响应包含 `base_id`、`changes`，每项为 `{"path":["对象键",...],"value":新值}` 或 `{"path":[...],"remove":true}`。对象按变化字段传送，数组作为一个字段整体替换；null 与删除不同。首次连接、管理进程重启、Router 重启或其他读取者替换基线时，返回完整 `state`。Router 只保留一份内存基线，管理端须保留原始基线用于差量合并，不能拿已清空消费的恢复快照作基线。合并和事务提交成功后才 ACK；丢失 ACK、重试或回退完整快照不重复结算。商户未变化时复用原加密恢复胶囊；只有变化商户才重新加密。

计划重启：管理平台暂停自己的同步任务，向 `/admin/quiesce` 提交当前 `boot_id` 和 `quiescing: true`，然后轮询 `/admin/metrics`，直到 `lifecycle.drained: true`。此时新业务、商户操作和配置写入返回 503 / `router_draining`，已经开始的请求以及所有商户的后台刷新完成后才视为排空。管理读取、checkpoint 与 ACK 仍可使用。保存最终 checkpoint 后停止 Router。停机前保存失败应提交 `quiescing: false` 恢复服务；云端停机结果不明时先确认状态，显式启动可以恢复仍在运行的暂停实例。暂停状态不写入快照；新进程必须重新初始化。不要用首次空初始化覆盖已有状态。

启用本地流水日志时，已提交的消费记录与待上传批次可以跨进程重启恢复；没有启用时仍有周期快照的崩溃丢失窗口。日志不包含账号凭证，refresh token 只保存在商户服务中；未生成或未提交的在途用量也不在恢复保证内。


## 供应商范围与升级

OAuth 采用 access-token-only-v1 协议，商户服务负责续期。Router 拒绝任何 refresh token 字段，不发起续期请求；尚未过期的 access token 保持可用。到期时间取 `expires_at`（Unix 秒）、Claude `expiresAt`（毫秒）或 JWT 的 `exp`；未提供且令牌不可解析时无法预告过期，只能在官方拒绝凭证后要求更新。

已过期账号在状态 API 中返回 `health.state: "expired"`、`health.last_error: "access_token_expired"` 和 `refresh_mode: "external"`，停止转发新请求；到期前被官方拒绝的凭证显示 `reauthorization_required`。重新导入完整的新凭证后恢复。refresh token 不得上传到 Router；Workbuddy API key 行为不变。

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

Router 内存保存上次同步余额 `balances` 和尚未同步的消费 `consumption`，请求只累加消费。新模型请求检查「余额 − 消费」，不足时返回 402 / `insufficient_balance`；用户尚未同步余额时返回 503 / `balance_unavailable`。同一用户的所有 key 共享余额，模型目录查询不受余额限制。已开始的并发请求仍会完成，因此可能出现负余额。

受管理的 Router 按上述可用余额降低用户共享并发：`0 < 余额 ≤ $1` 为 1，`$1 < 余额 ≤ $5` 为 2，`$5 < 余额 ≤ $10` 为 4，高于 $10 使用原用户上限。实际取阶梯与原用户上限的较小值；key、商户和全局限额继续独立生效。每次准入在余额及消费的同一把锁内选档并取得用户许可，无需请求管理平台。消费入账后立即降档，充值 ACK 后自动恢复。旧快照缺少配置时采用这些默认值，商户子引擎不重复执行平台余额限制。

`GET /admin/config` 的 `config.balance_concurrency` 返回当前规则；通过 `PATCH /admin/config` 热更新，例如：

```json
{"expected_revision":42,"patch":{"balance_concurrency":{"tiers":[
  {"up_to_microusd":1000000,"max_concurrent":1},
  {"up_to_microusd":5000000,"max_concurrent":2},
  {"up_to_microusd":10000000,"max_concurrent":4}
]}}}
```

金额为整数 micro-USD（1 USD = 1000000 micro-USD），1 至 5 档，阈值为正数且严格递增，并发上限不递减、最低档必须为 1。`GET /admin/users` 和 `GET /admin/users/{id}` 增加 `effective_max_concurrent`，表示用户并发与余额阶梯合并后的上限；余额未同步或耗尽时为 0。阶梯导致的拒绝返回 429 / `balance_concurrency_limit`，不会排队；余额耗尽仍返回 402。

降档不取消已经执行的请求。此机制降低同时在途请求造成的透支风险，不是费用预留或透支金额保证；在途请求及单次高费用请求仍可能使余额变负，尚待核算的费用也不计入已知消费。

`/admin/status` 和 `/admin/checkpoint` 的 `balance_sync_version: 1` 表示支持此协议。先升级管理端，再升级 Router；ACK 必须包含完整的 `balances`，可以为空，但未列出的用户不能发起模型请求。重复 ACK 不允许修改余额；充值等后续余额变动应读取新 checkpoint 后确认。官方价格计算与账单格式见 [billing v1](docs/BILLING.md)。

## 上游拒答审计

拒答审计默认启用，无需额外密钥。`/admin/status` 返回 `moderation_mode: "upstream_refusal"`、`lockllm_enabled: false`、`refusal_audit_enabled: true`。模型可用性仍受账号、目录、权限、余额、限流和记录队列容量约束。缺少 LockLLM 配置不再阻止推理或目录查询；旧快照里的 `config.moderation` 仅在导入时兼容读取，新导出省略，既不会调用扫描服务，也不要求恢复它的密钥。

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
