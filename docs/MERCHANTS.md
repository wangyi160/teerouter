# 多商户与统一平台 key

用户继续使用平台签发的 API key。`GET /v1/identity` 返回该 key 所属用户 ID；平台先授予商户接入资格，首次调用按商户的默认客户权限与限额执行。用户的全局并发、RPM、TPM 与停用状态跨商户生效。

商户只看到实际使用过本商户模型账号的客户。收到模型上游响应后（包含拒答及 HTTP 错误）才建立可见关系；浏览模型、平台预授权、本地校验拒绝和没有收到上游响应的连接失败都不会使用户可见。商户响应不含平台预授权的 `merchant.users`。关系独立于用量和拒答队列持久化，确认删除队列或重启后仍保留；旧版本按已有成功用量或拒答证据恢复，不能按预授权名单生成客户。

所有上游账号和账号池必须归属商户，平台不持有自营账号。商户拥有独立的账号、账号池、客户限额、价格、用量队列和拒答队列。平台管理员登记商户身份摘要、资源上限和允许接入的用户，不能通过平台账号导出商户凭证。平台配置和恢复快照中的根 `models`、`accounts` 必须为空，明文 secrets 只允许平台管理密钥和用户 key 的引用。旧快照须先将账号、代理、费率和访问范围迁入商户，再清理根配置；不能直接恢复含无归属账号池的快照。

## 身份和验证

商户客户端生成 32 字节随机数，编码为 64 字符十六进制 bearer token。`credential_hash` 为 token 文本的 SHA-256，商户 ID 为 `m_` 加摘要前 32 个十六进制字符。只向平台发送 ID 和摘要。平台通过 `PUT /admin/merchants` 登记：

```json
{"id":"m_<摘要前32字符>","credential_hash":"<64字符摘要>","name":"商户名称","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"allow_test":false,"users":["平台用户ID"]}
```

`GET /admin/merchants` 返回登记信息和解锁状态。平台可以停用商户、撤销用户或降低上限。商户 token 无权调用 `/admin/*`，平台管理员和用户 key 无权调用 `/merchant/*`。

商户须以自己审核的本地信任策略验证 TEE 证明、启动测量和当前 TLS 公钥绑定，再在**同一条已验证的 TLS 连接**发送 token、账号和设置。teeproxy 的 `merchant-transport --config PATH` 提供此受限私有管道，复用用户侧的验证器。它只允许下表中的商户接口，验证失败不发送凭证。信任策略不能从待验证服务器自动生成或接受。

## 商户接口

| 方法与路径 | 内容 |
| --- | --- |
| `POST /merchant/unlock` | `{"minimum_revision":0}`；解锁状态或恢复密文，低于本地确认版本拒绝恢复 |
| `GET /merchant/state` | 本商户设置、账号状态、账号池与目录；不返回账号凭证 |
| `PUT /merchant/config` | 完整配置更新，须带 `expected_revision`，冲突返回 409 |
| `POST /merchant/models/refresh` | 刷新官方模型目录 |
| `GET /merchant/usage?after_seq=0&limit=500` | 本商户用量队列及实例 ID |
| `POST /merchant/usage/ack` | `{"instance_id":"…","through_seq":123}`；删除已持久化的前缀 |
| `GET /merchant/refusals?after_id=0&limit=500` | 本商户递增 ID 的拒答队列 |
| `DELETE /merchant/refusals?through_id=123` | 删除该 ID 及以前的拒答记录 |

完整配置包含 `pools`、`pool_pricing`、`max_concurrent`、`rpm`、`tpm`，可用 `customer_defaults` 配置默认客户的 `enabled`、`models`、`max_concurrent`、`rpm`、`tpm`。默认允许本商户全部模型、并发 16、RPM/TPM 不限，仍受平台和商户总限额约束。`customers` 是已使用客户的权限更新列表，只接受已建立可见关系的用户；空列表或省略某个客户不删除已有权限，停用须显式提交 `enabled:false`。不能靠猜测用户 ID 预先添加未使用客户。可同时提交新增或轮换的 `accounts`（OAuth 账号）和 `secrets`（API key），以及 `delete_accounts`、`delete_secrets`。账号导入格式与现有 router `Provision` 相同。`merchant_internal_` 为保留密钥前缀。

```json
{
  "expected_revision":1,"max_concurrent":16,"rpm":60,"tpm":100000,
  "pools":[{"name":"openai","provider":"openai","deployments":[{"id":"account1","provider":"openai","api_key_ref":"upstream1","proxy":"socks5h://YOUR_ACCOUNT_PROXY:1080"}]}],
  "customer_defaults":{"enabled":true,"models":[],"max_concurrent":4,"rpm":30,"tpm":50000},
  "customers":[],
  "pool_pricing":[{"pool":"openai","multiplier_millionths":200000}],
  "secrets":{"upstream1":"ADMINISTER_THROUGH_ATTESTED_CHANNEL"}
}
```

商户只设置**每个账号池的倍率**：`multiplier_millionths: 1000000` 表示 1×，`200000` 表示 0.2×。支持 0 至 1,000,000,000×，精确到 6 位小数。输入、输出、缓存、长上下文阶梯统一乘以此倍率，账务在汇总用量后只做一次整数向上取整。非空旧 `prices` 更新将被拒绝。

平台管理员通过 `GET/PUT /admin/pricing` 维护官方基准价格。PUT 包含 `expected_revision` 和 `providers`；每项包含 `provider`、递增 `version`、官方 `source`、`checked_at` 和 `models`。模型包含 `model`、`rates`（整数 micro-USD / 百万 tokens）、可选 `long_context` 和 `valid_until`。价格内容有变化必须提升版本；不能回退版本或删除整个供应商价格簿。Workbuddy 的 source 必须为 `local-test:workbuddy`，仅为测试基准。平台需跟随官方调价更新价格簿。

调用事件的 `merchant_pricing` 包含 `basis: official_pool_multiplier`、**未经倍率乘算的基准费率**、`multiplier_millionths`、`official_price_revision`、`official_version`、`official_source`、`official_checked_at`。外部账务使用这份不可变快照；不能把基准费率直接视为销售费率。缺少池倍率、模型基准价或基准价过期时拒绝模型调用；缺少实际使用的缓存类别费率时保留待结算记录。恢复旧商户胶囊时，平台须先加载匹配的官方价格簿；只有全池所有模型、缓存、长上下文费率都能精确还原为同一倍率时才自动迁移，否则拒绝解锁。

每个启用的账号须指定网络出口代理（`http://`、`https://` 或 `socks5h://`）。API key 账号在 deployment 的 `proxy` 中配置；OAuth 账号在 `accounts` 对象内配置 `proxy`。代理仅提供网络出口，模型请求目标仍固定为官方服务。仅获授权的 Workbuddy 测试账号允许 `direct://`。

## 用户选路

推荐使用 [请求路由规则 v1](API.md#请求路由规则-v1)：客户端发送固定池或自动匹配规则、倍率上限与费率/速率优先策略，由 Router 在 TEE 内跨授权商户匹配并检查。原始模型 ID 可配合自动规则使用，客户端无需逐次读取报价或自行测速。响应头返回实际商户、账号池、倍率和报价版本。

`GET /v1/models` 返回 `m_<id>/官方模型ID`，并附商户、价格和 `m_<id>.账号池` 信息。可直接将返回的模型 ID 交给 agent；有多个可用池时，显式设置 `x-router-pool: m_<id>.账号池`。也可配合该请求头使用原始官方模型 ID。router 在请求上游前移除商户命名空间，上游响应保留官方模型 ID。

`GET /v1/merchants` 只返回当前用户有资格访问的商户及其 key 可见价格；`GET /v1/pools` 返回允许的账号池状态、商户名称、`pool_name` 和 `multiplier_millionths`。可选 `x-router-price-version` 固定商户倍率版本；可选 `x-router-official-price-revision` 固定平台基准价版本，不一致返回 `409 price_changed`。请求与响应头中均可追踪商户和版本。

未提供路由规则且不带商户标识的模型调用返回 `400 merchant_required`，不会走平台直连或隐式兜底。`/admin/accounts*` 和 `/admin/models*` 已关闭；账号操作与发现只在商户通道执行。商户配置可提交 `compatibility` 和 `restrictions`，沿用相同的官方域名、请求内容保护及只能收紧权限的校验。

商户不能设定自定义上游 URL、读取服务器 OAuth 文件或配置池间 fallback。正常服务仅允许编译固定的 OpenAI、Anthropic、xAI 官方域名。Workbuddy 同时需要构建特性和平台 `allow_test`，使用编译固定的测试地址；不会作为官方池的自动 fallback。

## 恢复、归档与边界

每个商户有独立内存 runtime。平台快照中的商户内容是 ChaCha20-Poly1305 加密的恢复胶囊；密钥由高熵商户 token 加域分离派生，AAD 绑定商户 ID，随机 nonce。平台保存密文，重启后须由商户重新验证 TEE 并解锁。平台停用不删除备份。凭证丢失将无法解锁，商户必须保护本机身份和恢复材料。

商户和平台分别接收用量及拒答事件。商户只能查询自己的记录；平台仍可审计平台用户跨商户的调用。拒答只保存明确拒答信号、原文及元数据，不保存请求正文。商户删除自身队列不会删除平台队列；平台也不能通过全局前缀删除影响商户队列。

本地管理端应先提交持久化事务，再确认队列前缀。队列积压至限制时暂停新调用。TPM 根据已观察到的完成用量计算，正在进行的请求可能造成短时超出。周期快照仍有崩溃丢失窗口；配置版本可检测旧配置，不能证明平台没有扣留最新审计数据。

恢复胶囊保护商户状态对平台保密；商户持有恢复密钥，它不是独立、不可伪造的金融凭证。本版没有第三方可验证的签名结算证明、在线收付款或完整反回滚硬件计数器。TEE 信任仍依赖客户端审核的镜像和本地策略；平台可停止服务，但不能令审核过的客户端接受任意替换镜像。
