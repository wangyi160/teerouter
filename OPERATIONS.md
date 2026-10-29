# Router 状态与同步协议

本文描述 Router 接收控制请求后的状态变化。所有 `/admin/*` 接口需要管理员 Bearer；用户和商户凭据不能读取根检查点或初始化根 runtime。

## 初始化

每次进程启动生成新的 `boot_id`，业务在成功初始化前不可用。`POST /admin/checkpoint/diff` 无有效基线时返回当前完整 `state` 和启动标识。

`POST /admin/initialize` 接收：

```json
{"boot_id":"当前启动标识","state":null}
```

`state:null` 表示空状态初始化；恢复使用完整且符合当前 schema 的 `state`。Router 校验启动标识、配置、身份、序号、余额与 relay 边界，每次启动只允许成功初始化一次。商户账号秘密不包含在根检查点中，商户 runtime 通过直接账号同步建立私有状态。

恢复输入不得携带未结算的 relay 汇总：`retained` 必须为空，`uploaded` 与 `settled` 必须一致；边界还须符合用量序号。启用认证日志时，Router 再校验日志与恢复边界并恢复已提交的尾部。恢复失败不替换为新的空账本。

## 差量检查点

`POST /admin/checkpoint/diff` 只接受 `boot_id`、`base_id`，首次均可为 null。响应包含 `checkpoint_diff_version:1`、新的 `checkpoint_id`，并返回完整 `state` 或基于 `base_id` 的 `changes`：

```json
{"path":["对象键","子键"],"value":"新值"}
```

删除用 `{"path":["对象键"],"remove":true}` 表示。对象按字段比较；数组整体替换；值 null 与删除不同。进程重启、基线缺失或基线被另一读取替换时，返回完整状态。Router 只保留一份差分基线。

检查点读取不创建私有签名批次、不确认上传，也不结算消费。`sign_receipts` 是未知字段，返回 400；旧的完整检查点 GET/ACK 接口不存在。

`storage.manager_connected` 是保留的状态字段名：最近 630 秒有合法差分快照读取时为 true。它不参与业务准入，不是心跳租约。

## 结算与余额

可用余额为最近同步余额减去未结算消费。模型请求只累加消费；上传确认只释放相应明细，不能更新余额或清除消费。

`POST /admin/receipts/relay/settle` 接收当前 `boot_id`、递增 `revision`、已上传批次边界 `through`，以及 `balances` 或 `balances_patch` 两者之一。Router 原子更新同步余额并清除边界内消费，保留之后的新消费。完全相同的重试幂等；旧版本、内容冲突、未知边界和错误启动标识被拒绝。具体签名与批次格式见 [消费凭据](docs/CONSUMPTION-PROOFS.md)。

用户及权限通过 `/admin/apply` 原子更新。合并补丁中的 null 表示删除字段；完整配置仍使用严格字段校验。配置更新和结算使用不同版本与状态边界。

## 排空状态

`POST /admin/quiesce` 接收 `{"boot_id":"当前值","quiescing":true}`。排空期间，新模型请求、商户操作和配置写入返回 `503 router_draining`；已有请求和已开始的后台任务继续结束。控制读取、检查点、relay flush 与结算仍可用。

`/admin/metrics` 的 `lifecycle.drained:true` 表示根及商户 runtime 均无在途请求或后台任务。提交 `quiescing:false` 恢复准入。该状态只在当前进程有效，不写入恢复检查点。

## 凭据与拒绝边界

Router 使用 `access-token-only-v1`，不接受 refresh token，也不发起 OAuth 续期。到期时间取 `expires_at`（Unix 秒）、Claude `expiresAt`（毫秒）或 JWT `exp`；没有可解析到期时间时无法预告过期。

到期账号显示 `health.state:"expired"`、`health.last_error:"access_token_expired"` 和 `refresh_mode:"external"`，停止接收新请求。提前被官方拒绝的凭据显示 `reauthorization_required`。新的 access 凭据须通过商户账号接口校验后生效。

提供商仅接受 `openai`、`anthropic`（别名 `claude`）、`xai`（别名 `grok`）。完整配置拒绝 `gemini_project`、`client_secret_ref`、`config.moderation` 和商户 `backup`，包括 null。旧快照中的空 `state.moderation:{}` 可读入但不再导出；非空历史明确拒绝，避免静默丢弃。旧 journal 中空 `receipt_head:null` 的读取规则见 [消费凭据](docs/CONSUMPTION-PROOFS.md)。

## 上游拒答审计

拒答识别默认启用；状态为 `moderation_mode:"upstream_refusal"`、`refusal_audit_enabled:true`。普通自然语言拒答不做关键词推断。明确拒答保留原始响应状态且不换账号重试。

完成、失败、超时或取消时，拒答类别与用量一并记录。公开批次只含 `refusal_type`，不含拒答原文。商户计数独立于逐笔明细保存。历史拒答队列仍进行恢复校验，但不提供独立 HTTP 查询/删除接口。

## 耐久性边界

无本地日志时，未同步的内存状态可能在进程异常退出后丢失。认证日志只能恢复成功提交的记录，不能补造未生成或未提交的在途用量；写入失败关闭新请求准入，但未提交尾部仍可能丢失。详见 [流水日志](docs/ACCOUNTING-JOURNAL.md) 和 [异常计费](docs/INTERRUPTED-BILLING.md)。
