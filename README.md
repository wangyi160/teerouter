# teerouter

运行在 TEE 中的精简 Rust 大模型 router。此仓库包含 Router 源码、构建依赖、测试和协议文档。
部署脚本、容器构建文件、云平台配置与运维工具由运营方独立维护，不随此仓库发布。管理平台、客户端 SDK 和演示页面属于独立项目。
另提供可独立构建的 [消费流水校验器](verifier/README.md)，用于核验 Router 签名凭据和 TEE 证明，无需安装桌面客户端。

管理服务可通过 `GET /admin/public-catalog` 读取供公开 Portal 展示的账号池目录。接口仍要求平台管理认证，但响应仅包含启用且已解锁商户的池名称、模型提供商、商户名称、账号聚合数量、并发占用/容量、商户共享限额及开放模型的官方基准价格和池倍率。模型目录遵守池级开放名单、官方目录有效期与模型限制；不返回账号明细、凭证、代理、用户关系或原始错误。不触发上游刷新、不建立客户关系、不改变调用授权。并发和限速是采样状态，商户额度由各池共享；报价缺失、过期或商户限速耗尽时不报告可用容量。

## 能力

官方模型目录按 OpenAI、Claude、Grok 共享，每个提供商默认每 300 秒刷新一份，候选来源覆盖所有启用商户的启用账号池。通过一个可用账号的代理拉取，失败时换其他账号；本轮没有成功来源（含全部失效、网络失败或超时）则清空该提供商目录，下个周期再获取。手动刷新使用同一份目录。各池开放模型名单、账号健康状态和调用权限仍独立；目录不含跨商户的账号身份、代理或凭证。

账号同步只校验配置、凭证结构与权限，不对每个账号发起官方连通性测试。`POST /merchant/accounts/test` 是独立测试接口；共享目录就绪不等于所有账号都已通过实测。失败清空目录不会删除商户保存的开放模型名单。

- OpenAI / ChatGPT、Claude / Anthropic、Grok / xAI 三类正式上游；外部导入凭证、多账号池、每账号代理、自动模型发现。
- Chat Completions、Responses、Messages 原生接口；保留 Agent 请求参数，不补参、删参或转换生成协议。
- 统一平台 API key、商户授权与用户并发控制；低余额按可配置阶梯降至 1 并发，余额耗尽停止新请求。全部上游账号归属商户，平台没有自营池；商户发布费率，外部管理服务结算。
- 请求路由规则 v1：客户端提交固定池或自动匹配、倍率上限及费率/低延迟优先规则，router 统一选路、检查并返回实际池与倍率。费率比较含缓存读写的预计费用；链路延迟由 router 每 5 分钟通过账号代理轻量探测官方入口，不产生模型 Token 用量，选路保留会话粘性。协议见 [API 文档](docs/API.md#请求路由规则-v1)。
- 上游拒答审计：识别明确的 refusal / content_filter 信号，将拒答原文、原因、用户标识和元数据写入全局递增 ID 队列；管理员按游标拉取，归档后按 ID 批量删除。请求正文不保存，不再调用 LockLLM。
- 官方域名固定在代码中；模型与思考参数对照官方目录验证并原值传递；管理员只能屏蔽，不能伪造目录或覆盖请求内容。
- Phala / dstack Intel TDX 证明与 TLS 1.3 在同一进程中完成；另保留 Linux SNP 取证路径。
- 业务配置通过管理 API 初始化和同步；Router 按提供商计算逐请求费用，以同步余额减去未同步消费检查余款。可启用 [TEE 加密本地流水日志](docs/ACCOUNTING-JOURNAL.md)，逐笔持久化账务状态；管理平台负责余额和结算，Router 不包含管理页面。
- [消费流水存证](docs/CONSUMPTION-PROOFS.md)：冻结批次、生成 Merkle 摘要与 Ed25519 签名，公钥绑定 TEE 证明。默认每 5 分钟上传，失败保留到下一周期；云端确认保存后删除本地明细，未结算消费摘要继续保留。不包括充值、提现等支付操作。

代码源于 LiteLLM 的选定模块移植与新建 Rust 集成，并非 LiteLLM 全量功能移植。来源、修改和协议限制见 [NOTICE](NOTICE) 与 [UPSTREAM.json](UPSTREAM.json)。

## 仓库范围

| 路径 | 内容 |
| --- | --- |
| `src/`、`tests/` | router 源码与自身测试 |
| `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` | 独立 Rust 构建与依赖锁定 |
| `config*.example.toml`、`*.example.json` | router 配置和管理 API 请求示例 |
| `egress/` | Router 使用的 VLESS 网络出口组件源码 |
| `verifier/` | 消费流水协议的独立校验器与测试 |
| `docs/`、根目录文档和许可证 | router 接口、运行、信任边界及来源说明 |

管理 API 是 router 的配置、权限执行和状态同步接口，保留在核心中；逐请求费用按 [计费协议](docs/BILLING.md) 在 router 内计算。管理页面、管理平台数据库、充值与持久化结算不属于本仓库；本地流水日志属于 Router 核心。构建和测试无需安装管理平台或客户端。

## 构建

仓库名为 teerouter，当前 Rust crate 和可执行文件名保留 `teellm`。

```sh
cargo build --release --locked --features dstack-tdx
```

冷启动等待管理 API 初始化。外部管理平台主动同步、持久化并确认快照；初始化后的服务准入与待确认队列容量见 [运行协议](OPERATIONS.md)。

真实硬件取证接口、审核后的 OS / 启动链及客户端信任策略属于 TEE 信任边界，普通主机不能产生有效硬件证明。`sample-demo` 为默认关闭的开发特性。上游地址固定为三家官方 HTTPS 域名，不能通过配置或编译特性添加测试服务。

## 初始化

首次通过 `POST /admin/checkpoint/diff` 获取 boot_id，再调用 `/admin/initialize`；平台通过 `/admin/apply` 同步用户权限。商户凭据只经直接商户通道上传，管理平台只读公开快照。用户模型请求必须同时携带平台授权和 teeproxy 本地 Ed25519 签名。接口清单见 [API](docs/API.md)，签名协议见 [用户认证](docs/USER-AUTH.md)。

`provision*.example.json` 和 `config*.example.toml` 展示内部上游引擎结构，用于协议测试；含账号池的示例不能作为平台初始化配置。商户秘密使用直接账号同步协议；公开配置采用上述文档中的平台配置命令格式。Claude 消费者 token 刷新仍由商户外部管理，不声称已实现完整登录流程。

## 文档与验证

- [架构](ARCHITECTURE.md)、[运行和恢复](OPERATIONS.md)、[接口索引](docs/API.md)、[TEE 安全边界](SECURITY-TEE.md)。
- [多商户、统一平台 key 与商户 TEE 通道](docs/MERCHANTS.md)。商户工作台独立于 router 和平台管理端。
- [上游拒答记录](docs/API.md#上游拒答队列)、[同步与保留范围](OPERATIONS.md#上游拒答审计)。无需额外安全 API 密钥；普通文本拒答不自动分类，未收到拒答信号不表示请求合规。
- 客户端需要独立实现或集成符合安全文档的证明验证与 TLS 绑定。

```sh
cargo test --locked --features sample-demo
cargo test --locked --no-default-features --features dstack-tdx
```

本地测试使用受控上游和模拟证据，不代表真实服务账号验证或生产安全审计。未启用本地日志时，周期同步存在崩溃丢失窗口；启用后的耐久性及磁盘回滚边界见 [本地流水日志](docs/ACCOUNTING-JOURNAL.md)。

许可证：MIT；依赖各自适用其许可证。
