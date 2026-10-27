# 多商户接入


商户只看到实际使用过本商户模型账号的客户。收到模型上游响应后（包含拒答及 HTTP 错误）才建立可见关系；浏览模型、平台预授权、本地校验拒绝和没有收到上游响应的连接失败都不会使用户可见。商户响应不含平台预授权的 `merchant.users`。可见关系保存在商户运行时，确认删除队列不会影响它。重启后关系由实际调用重新建立；商户本机保存的客户限额可在初次同步时恢复，但不能据此生成可见客户。

所有上游账号和账号池必须归属商户，平台不持有自营账号。商户拥有独立的账号、账号池、客户限额、价格和用量统计；逐笔流水统一进入平台账本，商户从管理平台读取自己的记录。平台管理员登记商户身份摘要、资源上限和允许接入的用户，不能通过平台账号导出商户凭证。平台配置和恢复快照中的根 `models`、`accounts` 必须为空，明文 secrets 只允许平台管理密钥和用户 key 的引用。旧快照须先将账号、代理、费率和访问范围迁入商户，再清理根配置；不能直接恢复含无归属账号池的快照。

## 身份和验证

接入凭证为 32 字节随机数的 64 字符小写十六进制编码，可由 Portal 签发。`credential_hash` 为接入凭证文本的 SHA-256，商户 ID 为 `m_` 加摘要前 32 个十六进制字符。平台通过 `PUT /admin/merchants` 登记：

```json
{"id":"m_<摘要前32字符>","credential_hash":"<64字符摘要>","name":"商户名称","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"allow_test":false,"users":["平台用户ID"]}
```

商户端另外生成独立的 32 字节控制密钥并在本机加密保存。商户通道的 Bearer 为 `<接入凭证>.<控制密钥>`；首次 `POST /merchant/bind` 将控制密钥摘要 `owner_hash` 绑定到此商户，后续请求必须同时验证两者。平台只保存摘要，不接收控制密钥；平台登记更新不能设置或替换已经绑定的摘要。接入凭证可在管理平台操作公开配置，但不能调用凭据私有通道。

`GET /admin/merchants` 返回登记信息和解锁状态。平台可以停用商户、撤销用户或降低上限。商户 token 无权调用 `/admin/*`，平台管理员和用户 key 无权调用 `/merchant/*`。首次绑定前，知道接入凭证的主体可能抢先绑定而造成拒绝服务，但不能获得尚未上传的账号秘密；商户必须确认绑定响应与本机控制密钥一致后才上传账号配置。

商户须以自己审核的本地信任策略验证 TEE 证明、启动测量和当前 TLS 公钥绑定，再在**同一条已验证的 TLS 连接**发送 token 和代理凭据。teemerchant 的内置验证组件提供此受限私有管道，只允许下表中的商户接口，验证失败不发送凭证。信任策略不能从待验证服务器自动生成或接受。

## 查询与配置通道

商户页面只从管理平台读取状态快照、模型目录、客户、收入、流水和拒答记录。管理平台每 5 分钟批量调用 `GET /admin/merchant-views` 更新自身存储；页面请求不触发 Router 查询，也没有直连回退。快照可以暂时落后，配置成功回执会立即更新平台快照。

已有账号池的倍率、开放模型和客户限制由平台调用 `PUT /admin/merchants/{id}/config` 下发，`operation_id` 提供幂等，`runtime_id` 与 `config.expected_revision` 提供并发控制。该公开通道拒绝账号凭据和明文代理。

账号新增、编辑、停用、删除通过 `PUT /merchant/accounts/sync` 直接提交到 TEE，Router 当场校验并激活，不需要平台提交票据或确认。输入包含 `operation_id`、`runtime_id`、`expected_revision`、`pools`、`accounts`、`secrets`、`delete_accounts` 和 `delete_secrets`。第一次发布非空账号池时可携带初始池设置及 `new_pool_pricing`；已有池的模型策略、倍率及商户/客户限制保持不变，须另外通过平台配置通道修改。refresh token 禁止上传。相同操作编号及内容重复提交不会再次应用。

商户收到 Router 成功回包后，在后台向管理平台发送只含运行时编号和版本号的成功通知。平台再通过 `GET /admin/merchants/{id}/view` 获取该商户公开快照。通知不携带凭据，也不作为账号成功的前提；失败由后续通知或五分钟批次补齐。页面请求仍只查询平台存储。旧 `accounts/prepare` 和 ticket 提交仅保留兼容，当前客户端不再使用。

## 商户接口

| 方法与路径 | 内容 |
| --- | --- |
| `POST /merchant/bind` | 绑定本机独立控制密钥；仅同一密钥可以重复绑定 |
| `POST /merchant/unlock` | `{"minimum_revision":0}`；创建商户内存状态，重启后 `configuration_required:true` 表示需要从本机同步账号配置 |
| `PUT /merchant/accounts/access` | 仅独立控制密钥认证的本商户可增量上传 access token，支持幂等重试；平台与其他商户无权操作 |
| `PUT /merchant/accounts/sync` | 直接校验、激活账号及池关联，返回成功状态；不依赖管理平台 |
| `POST /merchant/accounts/test` | 测试尚未保存的凭证与代理；不修改账号或配置 |

账号测试输入为 `{"pool":"已同步的本商户账号池","proxy":"socks5h://proxy.example:1080","credential":{"access_token":"…"}}`，也可用 `api_key` 替代 `credential`。Router 从账号池确定服务商，使用正式转发的官方客户端和代理访问模型目录，再选择一个已开放、有文本报价的模型发送最多 32 输出 token 的短请求；Workbuddy 测试池可使用目录中的文本模型。可选 `model` 指定待测模型。成功返回 `ok:true`、`model`、`model_count`、`provider` 和 `latency_ms`；失败返回 `ok:false`、`stage`、脱敏错误 `code` 及可用的上游状态码。列出模型成功不代表测试成功，模型生成也必须成功。

测试总超时 75 秒，每商户同时最多一次、全局最多八次，凭证只进入临时内存，不保存、不刷新 refresh token，不记入用户消费或商户收入。上游可能收取这条短请求的实际费用。正式账号必须填写独立代理，不允许 `direct://`，只有授权且编译启用的 Workbuddy 测试池例外。测试结果仅证明所测模型当时可用。

公开 config 包含 `pools`、`pool_pricing`、`max_concurrent`、`rpm`、`tpm`，可用 `customer_defaults` 配置默认客户的 `enabled`、`models`、`max_concurrent`、`rpm`、`tpm`。默认允许本商户全部模型、并发 16、RPM/TPM 不限，仍受平台和商户总限额约束。`customers` 是已使用客户的权限更新列表，只接受已建立可见关系的用户；空列表或省略某个客户不删除已有权限，停用须显式提交 `enabled:false`。不能靠猜测用户 ID 预先添加未使用客户。账号通道单独提交新增或轮换的 `accounts`（OAuth 账号）和 `secrets`（API key），以及 `delete_accounts`、`delete_secrets`。账号导入格式与现有 router `Provision` 相同。`merchant_internal_` 为保留密钥前缀。

```json
{
  "expected_revision":1,"max_concurrent":16,"rpm":60,"tpm":100000,
  "pools":[{"name":"openai","provider":"openai","deployments":[{"id":"account1","provider":"openai","api_key_ref":"upstream1","proxy":"socks5h://YOUR_ACCOUNT_PROXY:1080"}]}],
  "customer_defaults":{"enabled":true,"models":[],"max_concurrent":4,"rpm":30,"tpm":50000},
  "customers":[],
  "pool_pricing":[{"pool":"openai","multiplier_millionths":200000}]
}
```

商户只设置**每个账号池的倍率**：`multiplier_millionths: 1000000` 表示 1×，`200000` 表示 0.2×。支持 0 至 1,000,000,000×，精确到 6 位小数。输入、输出、缓存、长上下文阶梯统一乘以此倍率，账务在汇总用量后只做一次整数向上取整。非空旧 `prices` 更新将被拒绝。

平台管理员通过 `GET/PUT /admin/pricing` 维护官方基准价格。PUT 包含 `expected_revision` 和 `providers`；每项包含 `provider`、递增 `version`、官方 `source`、`checked_at` 和 `models`。模型包含 `model`、`rates`（整数 micro-USD / 百万 tokens）、可选 `long_context` 和 `valid_until`。价格内容有变化必须提升版本；不能回退版本或删除整个供应商价格簿。Workbuddy 的 source 必须为 `local-test:workbuddy`，仅为测试基准。平台需跟随官方调价更新价格簿。

调用事件的 `merchant_pricing` 包含 `basis: official_pool_multiplier`、**未经倍率乘算的基准费率**、`multiplier_millionths`、`official_price_revision`、`official_version`、`official_source`、`official_checked_at` 和 `billing_rules`。Router 按提供商分别计算，将结果与分项写入事件的 `billing`；管理平台校验后入账，不重新套用通用 Token 公式。报价和规则在请求开始时冻结。缺少池倍率、模型基准价或基准价过期时拒绝模型调用；缺少实际使用的缓存类别费率时保留待结算记录。重启恢复时，平台先加载官方价格簿，商户再从本机同步账号池倍率。详细字段、支持范围与拒绝策略见 [计费协议](BILLING.md)。

每个启用的账号须指定网络出口代理（`http://`、`https://` 或 `socks5h://`）。API key 部署的代理通过私有 `material.proxies` 提交；OAuth 账号在私有 `material.accounts` 内配置 `proxy`。公开配置只保留 `[redacted]` 代理占位。代理仅提供网络出口，模型请求目标仍固定为官方服务。仅获授权的 Workbuddy 测试账号允许 `direct://`。

## 账号池对外模型名单

管理平台配置命令 `config` 中的每个 `pools[]` 可设置 `allowed_models`，填写官方原始模型 ID（不加 `chatgpt/`、`claude/`、`grok/` 展示前缀）。例如池内账号实际支持 20 个模型，可只设置 `"allowed_models": ["model-a", "model-b", "model-c"]`。

名单属于账号池，池内所有账号共用；账号不支持单独配置名单。省略或 `null` 表示开放全部经过官方目录验证的模型，保留旧配置行为；`[]` 表示不开放任何模型。明确设置名单后，新发现的模型不会自动开放。最多 4096 个有效且不重复的 ID，不支持通配符，也不会自动放行其他别名。

管理平台商户状态快照返回该名单和未经过名单过滤的官方模型目录，供商户勾选。管理平台的模型目录、账号池模型计数，以及 Router 的自动选池和指定池请求都执行这项限制，并继续执行官方能力、用户授权和调用时的计费校验；写入一个不存在的模型不会使其可用。公开策略由管理平台下发，按配置版本更新，由商户本机保存恢复资料。修改应用于之后受理的请求，已经受理的请求继续使用受理时的配置。

正式上游的模型目录按提供商共享：OpenAI、Claude、Grok 各一份，覆盖所有启用商户的所有账号池。池内账号仍须属于同一提供商，并由商户保证提供相同的模型服务。Router 通过一个启用且未判定失效账号的凭证和代理查询官方目录，成功即停止；优先使用上次成功的来源，失败时可切换其他池、其他商户的账号。不逐账号拉取，不合并不同来源的结果。共享的仅是官方模型及能力，账号凭证、代理、健康、限额及各池开放名单保持独立。Workbuddy 测试池仍按池隔离。

平台的 `discovery.refresh_secs` 默认 300，即每 5 分钟到期刷新。后台最多每 30 秒检查一次，无需用户查询模型列表；商户「更新目录」只读取管理平台已保存的快照，不强制访问 Router 或官方。一次刷新使用 `discovery.timeout_secs` 的总时间预算（默认 15 秒），存在备用账号时单次尝试最多使用一半预算；未尝试完的来源在下一轮继续轮换。并发刷新合并为同一任务。若全部候选失败、没有候选或时间预算内未取得完整成功结果，立即清空该提供商目录，返回 `complete_catalog:false`，下一周期再尝试。没有有效目录时不得以旧目录接收请求。商户客户端应保留本地开放名单，不能把失败导致的空目录当成官方已下架全部模型。

正式上游目录状态为 `scope:"provider"`，`provider` 为 `openai`、`anthropic` 或 `xai`，并带 `refresh_secs`；不返回来源商户或 `source_deployment`。Workbuddy 保留 `scope:"pool"`。`catalog.data` 仍按本商户账号返回条目，但同提供商条目引用相同目录和刷新状态。同步账号仅验证权限、配置及凭证格式，不自动逐账号请求官方；真实连通性测试使用独立的 `/merchant/accounts/test`。共享目录就绪不表示该条目对应的账号已经通过生成测试。

## 用户选路

推荐使用 [请求路由规则 v1](API.md#请求路由规则-v1)：客户端发送固定池或自动匹配规则、倍率上限与费率/低延迟优先策略，由 Router 在 TEE 内跨授权商户匹配并检查。Router 每 5 分钟通过各账号的代理探测官方入口，相同代理与入口共用结果，不提交模型生成请求。原始模型 ID 可配合自动规则使用，客户端无需逐次读取报价或自行测速。响应头返回实际商户、账号池、倍率和报价版本。

teeproxy 本地的 `GET /v1/models` 从管理平台读取并返回 `chatgpt/官方模型ID`、`claude/官方模型ID`、`grok/官方模型ID`；测试服务使用 `workbuddy/官方模型ID`。前缀标识模型提供商。同一提供商的同一模型跨商户合并为一项，不同提供商的同名模型分别展示。

模型项的 `router_provider` 是内部提供商类型（`openai` / `anthropic` / `xai` / `workbuddy`），`router_model` 是完整官方模型 ID。`router_pools`、`router_capabilities[].pool` 以及 `router_pricing.rates[].pool` 使用 `m_<id>.账号池`；每项费率带 `merchant_id` 和 `price_version`，`router_merchants` 列出对应商户及其 `pools`。多商户模型没有统一的商户报价版本。

将目录返回的模型 ID 直接交给 agent，配合 `X-Router-Routing` 自动匹配或固定账号池。Router 在 TEE 内先限定模型提供商，再检查池、授权和倍率；固定池的提供商不匹配时也拒绝请求。转发上游时只移除提供商前缀，上游响应保留官方模型 ID。旧的原始模型 ID 和 `m_<id>/官方模型ID` 继续兼容；没有路由规则时仍需 `x-router-pool: m_<id>.账号池` 或旧商户前缀来确定商户。

用户通过管理平台查询有权限的商户、账号池、倍率及价格；Router 不再提供这些用户查询接口。调用时的上限和路由选择仍由 Router 执行。

未提供路由规则且不带商户标识的模型调用返回 `400 merchant_required`，不会走平台直连或隐式兜底。`/admin/accounts*` 和 `/admin/models*` 已关闭；账号操作与发现只在商户通道执行。商户配置可提交 `compatibility` 和 `restrictions`，沿用相同的官方域名、请求内容保护及只能收紧权限的校验。

商户不能设定自定义上游 URL、读取服务器 OAuth 文件或配置池间 fallback。正常服务仅允许编译固定的 OpenAI、Anthropic、xAI 官方域名。Workbuddy 同时需要构建特性和平台 `allow_test`，使用编译固定的测试地址；不会作为官方池的自动 fallback。

## 恢复、归档与边界

每个商户有独立内存 runtime。账号令牌、代理认证和私有配置只保存在商户本机及运行中的 TEE 内存；平台快照不包含商户秘密恢复胶囊，旧快照中的胶囊被忽略且不会再次导出。Router 重启后先由平台恢复公开登记及用户配置，再由商户验证 TEE、核对控制密钥绑定并从本机同步完整账号配置。商户必须保护本机身份和账号资料，平台不能代为恢复这些秘密。

商户服务负责续期并先保存最新凭据，再通过 TEE 通道上传 access token；Router 不持有 refresh token。

用量、计费和拒答事件只在平台账本生成一次。启用公开流水 relay 后，Router 生成签名批次并上传外部存储，管理平台校验、归档并结算，商户程序再从管理平台增量读取本商户记录。公开拒答只记录类型，不包含原文或请求正文；管理平台的商户接口不导出上游凭据或商户控制密钥。具体边界见 [商户流水](MERCHANT-LEDGER.md)。

商户读取账单不向 Router 发送归档确认。商户离线不会令第二份队列积压，也不会因此暂停模型服务；平台账本原有的容量、持久化和上传确认约束继续生效。TPM 根据已观察到的完成用量计算，正在进行的请求可能造成短时超出。未启用本地流水日志时仍有崩溃丢失窗口；配置版本可检测旧配置，不能证明平台没有扣留最新审计数据。

商户控制密钥与财务流水签名是不同的权限。流水持久化及签名见对应协议；这些记录不得携带上游账号凭据。TEE 信任仍依赖客户端审核的镜像和本地策略；平台可停止服务，但不能令审核过的客户端接受任意替换镜像。

## 拒答阈值

`customer_defaults.refusal_limit` 设置商户默认阈值，默认 `0` 不限制；单个客户的 `refusal_limit` 为 `null` 或省略时继承默认值，填 `0` 可单独关闭。阈值范围为 0–1,000,000,000。累计次数严格**超过**阈值后，Router 返回 HTTP 403 / `merchant_refusal_limit_exceeded`。例如阈值 3，第四次拒答后拒绝后续新请求；已转发的并发请求仍会完成。

累计按商户 ID 和平台用户 ID 归属，覆盖该用户全部 API key、账号池及模型，更换或重新生成 key 不会解除限制。自动路由会排除被限制商户，仍可选择用户有权限的其他商户；固定池不会绕过限制。只有上游明确的拒答信号计数，一次请求最多一次；普通网络错误、429 和普通 5xx 不计数。

管理平台商户状态快照 的 `customer_refusals` 只显示已使用客户的 `total`（历史总数）、`count`（清零后的次数）、`limit` 和 `blocked`。这些是准入控制统计，不是第二套流水接口。清零时提交该用户的 `refusal_baseline: total`，同时将 `enabled` 设为 `true`；基线单调递增，陈旧表单不能恢复已清零的次数，读取基线之后新增的拒答仍保留。

累计总数随平台账务统计保存，启用本地流水日志时在同一持久化事务写入；删除或上传逐笔记录不会清零。管理平台可在首次升级恢复时从已归档流水回填历史总数。商户的阈值及清零基线仍由商户程序本地保存，Router 重启后随商户配置恢复，不需要平台持有商户凭据。没有本地日志时，尚未同步的尾部统计仍有崩溃丢失窗口。

## Access-token-only v1

`merchant.credential_protocol` 为 `access-token-only-v1`。Router 不实现 OAuth 授权或续期，不接受或保存 refresh token。官方账号凭据仅支持 `access_token`、`expires_at`（Unix 秒）和可选 `account_id` 等明确的 access 元数据；`refresh_token`、`refreshToken`、空 refresh 字段、嵌套 refresh 字段与未识别字段均拒绝。规则覆盖初始化、账号导入、配置更新、停用账号和临时测试。兼容 Claude access-only 包装，但包装内不得含 refreshToken。Workbuddy 静态 API key 不变。

商户自行保存、续期完整凭据，经独立所有者密钥认证的 TEE 通道仅同步 access token。平台管理员不接触该通道。旧的凭据取回接口与 `/admin/accounts/{name}/refresh` 已删除，`operations.refresh_ahead_secs`、`oauth_refresh`、`oauth_discovery` 请求配置不再支持。

增量上传：`PUT /merchant/accounts/access`，请求：

```json
{"runtime_id":"from-merchant-state","accounts":{"alias":{"expected_access_sha256":"previous-access-token-sha256","credential":{"access_token":"NEW_ACCESS_TOKEN","expires_at":2000000000}}}}
```

完整处理在商户配置写锁内执行。旧实例返回 `merchant_runtime_changed`；旧摘要覆盖返回 `account_credential_changed`；带 refresh 等非法字段返回 `access_token_only`。完全相同的凭据重试直接返回成功，不增加账号版本。只更新已存在账号凭据，不改变池、客户或商户配置版本。返回商户状态及 `accounts.data[].access_sha256` 供重试核对。该摘要仅对账号所有者可见。

商户服务停止后，现有 access token 在有效期内仍可使用；过期或官方拒绝后停止该账号，等待商户上传更新。Router 重启后从商户恢复 access-only 配置，最新 refresh token 始终由商户保存。

用户请求需要平台授权和本地 Ed25519 签名，见 [USER-AUTH](USER-AUTH.md)。商户公开配置经管理平台下发；账号和代理秘密直接上传 Router。完整现行路由见 [API](API.md)。
