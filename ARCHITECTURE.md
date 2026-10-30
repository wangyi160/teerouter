<a id="router-code-structure"></a>

# Router 代码结构

**简体中文** | [English](ARCHITECTURE.en.md)

```text
HTTP 请求 → 认证与用户签名 → 权限、余额、限流 → 商户与账号选路 → 原生上游请求
                                                        ↓
响应或取消 → 用量与中断判定 → 费用事件 → 本地日志 → 签名批次与上传确认
                                      ↑
                            控制接口：配置、恢复、结算
```

<a id="request-and-account-isolation"></a>

## 请求与账号隔离

`src/server.rs` 定义 HTTP 路由及认证边界；`src/user_auth.rs` 验证用户签名和重放缓存。握手 JWT、用户 Bearer、管理员 Bearer 和商户所有者凭据具有不同权限。

根 runtime 保存用户授权、商户公开登记、全局用量和消费；上游账号与池只存在于商户 runtime。根配置拒绝账号池、账号凭据及出口代理。`src/merchants/` 负责商户身份、公开策略、私有账号同步和跨商户选路。公开控制接口不能导出商户秘密。

`src/router.rs` 和 `src/merchants/routing.rs` 执行权限、模型能力、并发、倍率与会话约束。配置热更新保留用户并发门；删除再建立同一用户不能绕过在途请求占用。降低上限只影响后续准入。

`src/client.rs` 与 `src/upstream.rs` 只实现原生转发；`src/sse.rs` 处理帧边界。请求与响应不经过 Chat→Claude 或 Claude→Chat 转换。官方目录由 `src/discovery/` 获取与验证，账号代理和官方域名限制由 `src/network/` 与 `src/official.rs` 执行。

<a id="billing-and-state"></a>

## 计费与状态

`src/accounting.rs` 管理请求和流的生命周期。`src/ledger.rs` 记录用量、费用与拒答类别；`src/billing/` 使用请求开始时冻结的报价，按 OpenAI、Anthropic、xAI 各自的计量语义计算费用。

`src/balances.rs` 按同步余额减去未结算消费检查可用金额。请求累加消费，不直接改写同步余额。`src/receipt_relay.rs` 把上传确认和结算确认分开；结算只清除确认边界内的消费，保留后续请求。详见 [计费协议](docs/BILLING.md)。

`src/vault.rs` 保存受互斥锁保护的内存状态；`src/vault/operations.rs` 处理版本比较、原子更新、凭据竞争和短期会话。可选 `src/vault/journal.rs` 以 SQLite WAL/FULL 事务保存账务状态，逐行认证加密并认证完整行集合，不记录上游凭证或请求正文。

`src/refusal.rs` 采集原生明确拒答信号；公开消费批次仅保存拒答类别，商户拒答次数单独累计。历史拒答队列的结构校验仍用于恢复，正常 relay 流程不新增原文队列。

<a id="lifecycle-and-attestation"></a>

## 生命周期与证明

`src/runtime.rs` 每次启动创建新的 `boot_id`。初始化恢复逻辑实例、配置和账务状态，但不恢复官方目录证据、健康状态或短期会话。旧进程命令不能确认新进程状态。

初始化后没有控制连接心跳租约。断联本身不暂停业务；余额、队列容量和日志健康约束仍生效。根与商户 runtime 共享显式排空标记和后台任务计数，只有 `/admin/quiesce` 修改该标记；标记不随检查点恢复。

`src/attestation.rs` 在同一进程内绑定 TLS 证书、随机挑战与 TEE 证据。`src/receipts.rs` 的独立进程签名密钥通过另一组 REPORT_DATA 绑定公开消费批次。信任范围见 [TEE 安全边界](SECURITY-TEE.md)。

文件、环境变量和 TOML 测试装配位于 `tests/config_fixture.rs`，不进入常规构建。根初始化与商户同步均通过已定义的协议校验输入。
