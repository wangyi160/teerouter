# 运行与恢复

router：`teellm --admin-key-stdin [--bind 127.0.0.1:4000] [--tls-bind 0.0.0.0:8443]`。标准输入是一行管理凭证；无需数据库文件或数据库密钥。持久化由外部管理服务负责。

1. 冷启动 `GET /admin/checkpoint` 返回新的 boot_id、initialized=false。
2. 管理端发送 `POST /admin/initialize`：`{"boot_id":"当前值","state":已保存的state或null}`。null 只用于首次空初始化；已有服务必须恢复管理数据库保存的快照。
3. 初始化仅允许一次，验证全部配置、凭证、管理员身份与账号版本；失败不激活候选状态。
4. 管理端读取 `/admin/checkpoint`，原子保存 state、用量和计费结果，然后 POST `/admin/checkpoint/ack`，包含同一 boot_id、generation、sequence。
5. ACK 只清除已同步用量，不清除读取快照之后产生的新用量。每轮同步重复此过程。密码、token 和完整快照必须经经过验证的保密连接传输，不放在日志中。

管理 API 沿用 Bearer 认证，快照响应禁止缓存；业务 API key 不能读取快照或初始化。账务 API 仅存在于独立管理平台，router 不接受充值、余额或定价命令。

每轮同步后等待 1 秒。管理确认超过 30 秒未更新时暂停新业务请求；已经开始的请求继续完成。达到待同步 usage 上限也暂停新请求，避免长期断联占满内存。模型目录查询同样要求管理连接健康。

计划重启：暂停新请求，等待在途请求结束，通过 `/admin/checkpoint` 保存最新状态并 ACK 后停止 router。新进程等待外部管理服务恢复；不要用首次空初始化覆盖已有状态。

注意：周期快照有崩溃丢失窗口，包括用量和轮换 refresh token。需要更强保证时必须引入同步持久化协议，当前精简版本没有承诺零丢失。


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
