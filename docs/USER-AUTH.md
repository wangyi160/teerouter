<a id="client-owned-user-request-signatures-v2"></a>

# 客户端持有密钥的用户请求签名 v2

**简体中文** | [English](USER-AUTH.en.md)

有效的用户 Bearer 授予访问权限，但单凭 Bearer 不能发起模型请求。Router 还会使用已登记的 `User.public_key` 验证 Ed25519 签名。`User.id` 必须为 `u_sig_` 加该 32 字节公钥的 SHA-256 小写十六进制摘要。配置写入会检查这一派生关系；同一 ID 更换公钥无法通过校验。Router 不接收用户签名种子。

<a id="wire-format"></a>

## 线上格式

必须提供以下单值请求头：

- `x-router-user-id`：已登记的身份；旧版公钥请求头会被拒绝。
- `x-router-user-timestamp`：规范十进制格式的 Unix 秒。
- `x-router-user-nonce`：全新随机生成的 32 字节值，小写十六进制编码。
- `x-router-user-signature`：64 字节 Ed25519 签名，小写十六进制编码。

对下列 UTF-8 紧凑 JSON 数组签名：

```text
["teerouter-user-request-v2", boot_id, user_id, method, path_and_query,
 timestamp_string, nonce_hex, sha256_body_hex, [[header_name, value_bytes_hex], ...]]
```

请求头名称转换为小写并排序；同名的多个值保留线上顺序。签名覆盖除 `x-router-user-*` 与以下传输字段之外的所有请求头：`host`、`content-length`、`connection`、`transfer-encoding`、`keep-alive`、`te`、`trailer`、`upgrade`、`proxy-authorization`、`proxy-connection`。`Connection` 只能包含 `close` 或 `keep-alive`，不能指定删除其他字段。授权、Agent 身份与路由请求头都在签名范围内。正文摘要使用 Router 在解析 JSON 前接收的原始字节，而不是重新序列化后的 JSON。

<a id="verification-and-replay-protection"></a>

## 验证与重放保护

签名证明绑定通过独立验证的 TLS 通道取得的当前 Router `boot_id`。证明在 300 秒后过期，允许的未来时钟偏差不超过 60 秒。验证成功后，随机数记入有容量上限的进程级共享缓存，配置更新不会重置该缓存；缓存饱和时拒绝新的证明。新启动会使此前全部证明失效。

签名错误或重放在选路和计费前返回 401。开始发送后，不自动重放已签名请求。用户 ID 也用于并发控制、商户客户限额、拒答计数和消费归属；Router 不维护第二套客户 ID 或身份别名。

握手 JWT 仅授权证明与健康协商，不授权模型或控制操作。管理员可改变权限、余额和服务准入，但不能替代有效用户签名。这一保证以实际执行的是经过验证的 Router 实现为前提。
