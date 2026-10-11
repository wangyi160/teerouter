# Router API


路径相对于 router 地址；业务请求须先由独立客户端验证 TLS / TEE。

用户接口使用用户 API key；管理接口使用独立管理密钥。认证头为 `Authorization: Bearer <KEY>`，兼容 `x-api-key` / `x-goog-api-key`，不接受 URL 中的 key。

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
| POST | `/v1beta/models/{model}:generateContent` | 模型名在 URL 中，请求体使用 contents。 |
| POST | `/v1beta/models/{model}:streamGenerateContent` | 模型名在 URL 中，返回流式数据。 |

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
| GET | `/admin/users` | 用户、所属 API key 与并发权限。 |
| POST | `/admin/users` | 创建用户及其访问权限；金额不属于 router 用户属性。 |
| GET | `/admin/users/{id}` | 读取指定用户。 |
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
| GET | `/admin/logs` | 运行元数据，不是问题与回答内容。 |
| POST | `/admin/maintenance` | 触发内部维护任务；正常使用由 router 自动执行。 |
