# 多商户与统一平台 key

用户继续使用平台签发的 API key。`GET /v1/identity` 返回该 key 所属用户 ID；平台先授予商户接入资格，首次调用按商户的默认客户权限与限额执行。用户的全局并发、RPM、TPM 与停用状态跨商户生效。

商户只看到实际使用过本商户模型账号的客户。收到模型上游响应后（包含拒答及 HTTP 错误）才建立可见关系；浏览模型、平台预授权、本地校验拒绝和没有收到上游响应的连接失败都不会使用户可见。商户响应不含平台预授权的 `merchant.users`。可见关系保存在商户运行时，确认删除队列不会影响它。重启后关系由实际调用重新建立；商户本机保存的客户限额可在初次同步时恢复，但不能据此生成可见客户。

所有上游账号和账号池必须归属商户，平台不持有自营账号。商户拥有独立的账号、账号池、客户限额、价格、用量队列和拒答队列。平台管理员登记商户身份摘要、资源上限和允许接入的用户，不能通过平台账号导出商户凭证。平台配置和恢复快照中的根 `models`、`accounts` 必须为空，明文 secrets 只允许平台管理密钥和用户 key 的引用。旧快照须先将账号、代理、费率和访问范围迁入商户，再清理根配置；不能直接恢复含无归属账号池的快照。

## 身份和验证

接入凭证为 32 字节随机数的 64 字符小写十六进制编码，可由 Portal 签发。`credential_hash` 为接入凭证文本的 SHA-256，商户 ID 为 `m_` 加摘要前 32 个十六进制字符。平台通过 `PUT /admin/merchants` 登记：

```json
{"id":"m_<摘要前32字符>","credential_hash":"<64字符摘要>","name":"商户名称","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"allow_test":false,"users":["平台用户ID"]}
```

商户端另外生成独立的 32 字节控制密钥并在本机加密保存。商户通道的 Bearer 为 `<接入凭证>.<控制密钥>`；首次 `POST /merchant/bind` 将控制密钥摘要 `owner_hash` 绑定到此商户，后续请求必须同时验证两者。平台只保存摘要，不接收控制密钥；平台登记更新不能设置或替换已经绑定的摘要。单独的接入凭证不能操作商户账号池。

`GET /admin/merchants` 返回登记信息和解锁状态。平台可以停用商户、撤销用户或降低上限。商户 token 无权调用 `/admin/*`，平台管理员和用户 key 无权调用 `/merchant/*`。首次绑定前，知道接入凭证的主体可能抢先绑定而造成拒绝服务，但不能获得尚未上传的账号秘密；商户必须确认绑定响应与本机控制密钥一致后才上传账号配置。

商户须以自己审核的本地信任策略验证 TEE 证明、启动测量和当前 TLS 公钥绑定，再在**同一条已验证的 TLS 连接**发送 token、账号和设置。teemerchant 的内置验证组件提供此受限私有管道，只允许下表中的商户接口，验证失败不发送凭证。信任策略不能从待验证服务器自动生成或接受。

## 商户接口

| 方法与路径 | 内容 |
| --- | --- |
| `POST /merchant/bind` | 绑定本机独立控制密钥；仅同一密钥可以重复绑定 |
| `POST /merchant/unlock` | `{"minimum_revision":0}`；创建商户内存状态，重启后 `configuration_required:true` 表示需要从本机同步账号配置 |
| `GET /merchant/state` | 本商户设置、账号状态、账号池与目录；不返回账号凭证 |
| `PUT /merchant/accounts/access` | 仅独立控制密钥认证的本商户可增量上传 access token，支持幂等重试；平台与其他商户无权操作 |
| `PUT /merchant/config` | 完整配置更新，须带 `expected_revision`，冲突返回 409 |
| `POST /merchant/models/refresh` | 刷新官方模型目录 |
| `POST /merchant/accounts/test` | 测试尚未保存的凭证与代理；不修改账号或配置 |
| `GET /merchant/usage?after_seq=0&limit=500` | 本商户用量队列及实例 ID |
| `POST /merchant/usage/ack` | `{"instance_id":"…","through_seq":123}`；删除已持久化的前缀 |
| `GET /merchant/refusals?after_id=0&limit=500` | 本商户递增 ID 的拒答队列 |
| `DELETE /merchant/refusals?through_id=123` | 删除该 ID 及以前的拒答记录 |

账号测试输入为 `{"pool":"已同步的本商户账号池","proxy":"socks5h://proxy.example:1080","credential":{"access_token":"…"}}`，也可用 `api_key` 替代 `credential`。Router 从账号池确定服务商，使用正式转发的官方客户端和代理访问模型目录，再选择一个已开放、有文本报价的模型发送最多 32 输出 token 的短请求；Workbuddy 测试池可使用目录中的文本模型。可选 `model` 指定待测模型。成功返回 `ok:true`、`model`、`model_count`、`provider` 和 `latency_ms`；失败返回 `ok:false`、`stage`、脱敏错误 `code` 及可用的上游状态码。列出模型成功不代表测试成功，模型生成也必须成功。

测试总超时 75 秒，每商户同时最多一次、全局最多八次，凭证只进入临时内存，不保存、不刷新 refresh token，不记入用户消费或商户收入。上游可能收取这条短请求的实际费用。正式账号必须填写独立代理，不允许 `direct://`，只有授权且编译启用的 Workbuddy 测试池例外。测试结果仅证明所测模型当时可用。

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

调用事件的 `merchant_pricing` 包含 `basis: official_pool_multiplier`、**未经倍率乘算的基准费率**、`multiplier_millionths`、`official_price_revision`、`official_version`、`official_source`、`official_checked_at` 和 `billing_rules`。Router 按提供商分别计算，将结果与分项写入事件的 `billing`；管理平台校验后入账，不重新套用通用 Token 公式。报价和规则在请求开始时冻结。缺少池倍率、模型基准价或基准价过期时拒绝模型调用；缺少实际使用的缓存类别费率时保留待结算记录。重启恢复时，平台先加载官方价格簿，商户再从本机同步账号池倍率。详细字段、支持范围与拒绝策略见 [计费协议](BILLING.md)。

每个启用的账号须指定网络出口代理（`http://`、`https://` 或 `socks5h://`）。API key 账号在 deployment 的 `proxy` 中配置；OAuth 账号在 `accounts` 对象内配置 `proxy`。代理仅提供网络出口，模型请求目标仍固定为官方服务。仅获授权的 Workbuddy 测试账号允许 `direct://`。

## 账号池对外模型名单

`PUT /merchant/config` 的每个 `pools[]` 可设置 `allowed_models`，填写官方原始模型 ID（不加 `chatgpt/`、`claude/`、`grok/` 展示前缀）。例如池内账号实际支持 20 个模型，可只设置 `"allowed_models": ["model-a", "model-b", "model-c"]`。

名单属于账号池，池内所有账号共用；账号不支持单独配置名单。省略或 `null` 表示开放全部经过官方目录验证的模型，保留旧配置行为；`[]` 表示不开放任何模型。明确设置名单后，新发现的模型不会自动开放。最多 4096 个有效且不重复的 ID，不支持通配符，也不会自动放行其他别名。

`GET /merchant/state` 返回该名单和未经过名单过滤的官方模型目录，供商户勾选。用户的 `/v1/models`、`/v1/pools` 模型计数、自动选池和指定池请求都执行这项限制，并继续执行官方能力、用户授权和调用时的计费校验；写入一个不存在的模型不会使其可用。策略经已验证的商户通道同步，按配置版本更新，由商户本机保存恢复资料。修改应用于之后受理的请求，已经受理的请求继续使用受理时的配置。

官方模型目录按账号池共享。池内账号必须属于同一模型提供商，并由商户保证提供相同的模型服务。Router 使用一个已启用、可用账号的凭证和代理查询官方目录，完整获取成功后供全池使用；不逐账号拉取，也不合并不同账号的结果。首次按池内账号顺序尝试，之后优先使用上次成功的账号；认证失败、网络错误、超时或返回无效目录时尝试池内其他账号，成功即停止。目录只表明池的模型与能力，实际调用仍检查账号健康、限额及上游响应。

默认 `discovery.refresh_secs = 300`，即每 5 分钟到期刷新。后台最多每 30 秒检查一次，无需用户查询模型列表；「更新目录」对应的接口立即强制刷新。一次刷新使用 `discovery.timeout_secs` 的总时间预算（默认 15 秒），存在备用账号时为单次尝试最多分配一半预算，避免一个超时账号耗尽整轮时间；未尝试完的账号在后续重试继续轮换。并发读取和强制刷新会共享同一任务，每轮每池最多保留一份完整结果。全部失败时不会拼接残缺目录或绕过过期检查；仍在有效期内的旧目录可继续使用，失效后停止以它接收请求。

目录状态新增 `scope: "pool"`、`source_deployment`（获取目录的账号别名）及 `refresh_secs`。为兼容现有客户端，`catalog.data` 仍按账号返回条目，但同池条目引用同一份目录和刷新状态，不表示每个账号都向官方发送了查询。

## 用户选路

推荐使用 [请求路由规则 v1](API.md#请求路由规则-v1)：客户端发送固定池或自动匹配规则、倍率上限与费率/低延迟优先策略，由 Router 在 TEE 内跨授权商户匹配并检查。Router 每 5 分钟通过各账号的代理探测官方入口，相同代理与入口共用结果，不提交模型生成请求。原始模型 ID 可配合自动规则使用，客户端无需逐次读取报价或自行测速。响应头返回实际商户、账号池、倍率和报价版本。

`GET /v1/models` 返回 `chatgpt/官方模型ID`、`claude/官方模型ID`、`grok/官方模型ID`；测试服务使用 `workbuddy/官方模型ID`。前缀标识模型提供商。同一提供商的同一模型跨商户合并为一项，不同提供商的同名模型分别展示。

模型项的 `router_provider` 是内部提供商类型（`openai` / `anthropic` / `xai` / `workbuddy`），`router_model` 是完整官方模型 ID。`router_pools`、`router_capabilities[].pool` 以及 `router_pricing.rates[].pool` 使用 `m_<id>.账号池`；每项费率带 `merchant_id` 和 `price_version`，`router_merchants` 列出对应商户及其 `pools`。多商户模型没有统一的商户报价版本。

将目录返回的模型 ID 直接交给 agent，配合 `X-Router-Routing` 自动匹配或固定账号池。Router 在 TEE 内先限定模型提供商，再检查池、授权和倍率；固定池的提供商不匹配时也拒绝请求。转发上游时只移除提供商前缀，上游响应保留官方模型 ID。旧的原始模型 ID 和 `m_<id>/官方模型ID` 继续兼容；没有路由规则时仍需 `x-router-pool: m_<id>.账号池` 或旧商户前缀来确定商户。

`GET /v1/merchants` 只返回当前用户有资格访问的商户及其 key 可见价格；`GET /v1/pools` 返回允许的账号池状态、商户名称、`pool_name` 和 `multiplier_millionths`。可选 `x-router-price-version` 固定商户倍率版本；可选 `x-router-official-price-revision` 固定平台基准价版本，不一致返回 `409 price_changed`。请求与响应头中均可追踪商户和版本。

未提供路由规则且不带商户标识的模型调用返回 `400 merchant_required`，不会走平台直连或隐式兜底。`/admin/accounts*` 和 `/admin/models*` 已关闭；账号操作与发现只在商户通道执行。商户配置可提交 `compatibility` 和 `restrictions`，沿用相同的官方域名、请求内容保护及只能收紧权限的校验。

商户不能设定自定义上游 URL、读取服务器 OAuth 文件或配置池间 fallback。正常服务仅允许编译固定的 OpenAI、Anthropic、xAI 官方域名。Workbuddy 同时需要构建特性和平台 `allow_test`，使用编译固定的测试地址；不会作为官方池的自动 fallback。

## 恢复、归档与边界

每个商户有独立内存 runtime。账号令牌、代理认证和私有配置只保存在商户本机及运行中的 TEE 内存；平台快照不包含商户秘密恢复胶囊，旧快照中的胶囊被忽略且不会再次导出。Router 重启后先由平台恢复公开登记及用户配置，再由商户验证 TEE、核对控制密钥绑定并从本机同步完整账号配置。商户必须保护本机身份和账号资料，平台不能代为恢复这些秘密。

商户服务负责续期并先保存最新凭据，再通过 TEE 通道上传 access token；Router 不持有 refresh token。

商户和平台分别接收用量及拒答事件。商户只能查询自己的记录；平台仍可审计平台用户跨商户的调用。拒答只保存明确拒答信号、原文及元数据，不保存请求正文。商户删除自身队列不会删除平台队列；平台也不能通过全局前缀删除影响商户队列。

本地管理端应先提交持久化事务，再确认队列前缀。队列积压至限制时暂停新调用。TPM 根据已观察到的完成用量计算，正在进行的请求可能造成短时超出。周期快照仍有崩溃丢失窗口；配置版本可检测旧配置，不能证明平台没有扣留最新审计数据。

商户控制密钥与财务流水签名是不同的权限。流水持久化及签名见对应协议；这些记录不得携带上游账号凭据。TEE 信任仍依赖客户端审核的镜像和本地策略；平台可停止服务，但不能令审核过的客户端接受任意替换镜像。

## Access-token-only v1

`merchant.credential_protocol` 为 `access-token-only-v1`。Router 不实现 OAuth 授权或续期，不接受或保存 refresh token。官方账号凭据仅支持 `access_token`、`expires_at`（Unix 秒）和可选 `account_id` 等明确的 access 元数据；`refresh_token`、`refreshToken`、空 refresh 字段、嵌套 refresh 字段与未识别字段均拒绝。规则覆盖初始化、账号导入、配置更新、停用账号和临时测试。兼容 Claude access-only 包装，但包装内不得含 refreshToken。Workbuddy 静态 API key 不变。

商户自行保存、续期完整凭据，经独立所有者密钥认证的 TEE 通道仅同步 access token。平台管理员不接触该通道。旧的凭据取回接口与 `/admin/accounts/{name}/refresh` 已删除，`operations.refresh_ahead_secs`、`oauth_refresh`、`oauth_discovery` 请求配置不再支持。

增量上传：`PUT /merchant/accounts/access`，请求：

```json
{"runtime_id":"from-merchant-state","accounts":{"alias":{"expected_access_sha256":"previous-access-token-sha256","credential":{"access_token":"NEW_ACCESS_TOKEN","expires_at":2000000000}}}}
```

完整处理在商户配置写锁内执行。旧实例返回 `merchant_runtime_changed`；旧摘要覆盖返回 `account_credential_changed`；带 refresh 等非法字段返回 `access_token_only`。完全相同的凭据重试直接返回成功，不增加账号版本。只更新已存在账号凭据，不改变池、客户或商户配置版本。返回商户状态及 `accounts.data[].access_sha256` 供重试核对。该摘要仅对账号所有者可见。

商户服务停止后，现有 access token 在有效期内仍可使用；过期或官方拒绝后停止该账号，等待商户上传更新。Router 重启后从商户恢复 access-only 配置，最新 refresh token 始终由商户保存。
