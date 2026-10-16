# teerouter

运行在 TEE 中的精简 Rust 大模型 router。此仓库只包含 router、测试和部署材料。
管理平台、客户端 SDK 和演示页面属于独立项目，不随此仓库发布。

管理服务可通过 `GET /admin/public-catalog` 读取供公开 Portal 展示的账号池目录。接口仍要求平台管理认证，但响应仅包含启用且已解锁商户的池名称、模型提供商、商户名称、账号聚合数量、并发占用/容量、商户共享限额及开放模型的官方基准价格和池倍率。模型目录遵守池级开放名单、官方目录有效期与模型限制；不返回账号明细、凭证、代理、用户关系或原始错误。不触发上游刷新、不建立客户关系、不改变调用授权。并发和限速是采样状态，商户额度由各池共享；报价缺失、过期或商户限速耗尽时不报告可用容量。

## 能力

- OpenAI / ChatGPT、Claude / Anthropic、Grok / xAI 三类正式上游，另保留可选的 Workbuddy 测试上游；外部导入凭证、多账号池、每账号代理、自动模型发现。
- Chat Completions、Responses、Messages 接口；支持流式转发和选定格式转换。
- 统一平台 API key、商户授权与用户并发控制。全部上游账号归属商户，平台没有自营池；商户发布费率，外部管理服务结算。
- 请求路由规则 v1：客户端提交固定池或自动匹配、倍率上限及费率/速率优先规则，router 统一选路、检查并返回实际池与倍率。真实流式测速在 router 内进行，选路规则和请求正文分离。协议见 [API 文档](docs/API.md#请求路由规则-v1)。
- 上游拒答审计：识别明确的 refusal / content_filter 信号，将拒答原文、原因、用户标识和元数据写入全局递增 ID 队列；管理员按游标拉取，归档后按 ID 批量删除。请求正文不保存，不再调用 LockLLM。
- 官方域名固定在代码中；模型与思考参数对照官方目录验证并原值传递；管理员只能屏蔽，不能伪造目录或覆盖请求内容。
- Phala / dstack Intel TDX 证明与 TLS 1.3 在同一进程中完成；另保留 Linux SNP 取证路径。
- 全部业务状态保存在内存中，通过管理 API 初始化和同步；router 按提供商计算逐请求费用，不包含数据库、管理页面或余额结算系统。

代码源于 LiteLLM 的选定模块移植与新建 Rust 集成，并非 LiteLLM 全量功能移植。来源、修改和协议限制见 [NOTICE](NOTICE) 与 [UPSTREAM.json](UPSTREAM.json)。

## 仓库范围

| 路径 | 内容 |
| --- | --- |
| `src/`、`tests/` | router 源码与自身测试 |
| `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` | 独立 Rust 构建与依赖锁定 |
| `config*.example.toml`、`*.example.json` | router 配置和管理 API 请求示例 |
| `deployment/phala/` | router 容器、部署模板、构建验证工具 |
| `docs/`、根目录文档和许可证 | router 接口、运行、信任边界及来源说明 |

管理 API 是 router 的配置、权限执行和状态同步接口，保留在核心中；逐请求费用按 [计费协议](docs/BILLING.md) 在 router 内计算。管理页面、数据库、充值与持久化结算不属于本仓库。构建和测试无需安装管理平台或客户端。

## 构建与启动

仓库名为 teerouter，当前 Rust crate 和可执行文件名保留 `teellm`，以兼容已有部署。

```sh
cargo build --release --locked --features dstack-tdx
./target/release/teellm --admin-key-stdin --bind 127.0.0.1:4000 --tls-bind 0.0.0.0:8443
```

通过受保护标准输入提供一行至少 32 字符的管理凭证。也支持部署环境注入，见 [Phala 部署](deployment/phala/README.md)。冷启动等待管理 API 初始化，之后需要外部服务持续同步与确认快照；长期没有管理确认会暂停新请求。

生产运行需要真实硬件取证接口、审核后的 OS / 启动链 / 镜像和客户端信任策略。普通主机不能产生有效硬件证明。`sample-demo` 和 `workbuddy-lab` 均为默认关闭的开发特性。Workbuddy 默认固定为 `http://127.0.0.1:8001/v1`；兼容既有测试服务时，可以在构建时设置 `TEEROUTER_WORKBUDDY_BASE_URL`，容器还需显式设置 `ROUTER_FEATURES=dstack-tdx,workbuddy-lab`。该地址编译进二进制，只允许同源 `/v1` 路径，运行时不能修改，也不能替换其他供应商的官方地址。HTTP 测试上游没有传输加密；启用此功能的镜像应单独审核和固定摘要。Workbuddy 如返回兼容协议中的明确拒答信号，也会写入统一拒答队列。

## 初始化

读取 `/admin/checkpoint` 中的 `boot_id`，首次调用 `/admin/initialize` 并传入 `state: null`，再通过 `/admin/apply` 导入平台用户和 key。平台登记商户后，由商户验证 TEE、解锁并通过 `/merchant/config` 导入账号池和上游凭证。平台根配置禁止上游账号池和凭证；每次调用必须选择商户。重启时恢复平台快照中的商户密文，再由商户解锁。详见 [商户接入](docs/MERCHANTS.md)。

`provision*.example.json` 和 `config*.example.toml` 展示内部上游引擎结构，用于协议测试；含账号池的示例不能作为平台初始化配置。商户使用上述文档的 `/merchant/config` 格式。Claude 消费者 token 刷新仍由商户外部管理，不声称已实现完整登录流程。

## 文档与验证

- [架构](ARCHITECTURE.md)、[运行和恢复](OPERATIONS.md)、[接口索引](docs/API.md)、[TEE 安全边界](SECURITY-TEE.md)。
- [多商户、统一平台 key 与商户 TEE 通道](docs/MERCHANTS.md)。商户工作台独立于 router 和平台管理端。
- [上游拒答记录](docs/API.md#上游拒答队列)、[同步与保留范围](OPERATIONS.md#上游拒答审计)。无需额外安全 API 密钥；普通文本拒答不自动分类，未收到拒答信号不表示请求合规。
- [Phala 部署](deployment/phala/README.md)。客户端需要独立实现或集成符合安全文档的证明验证与 TLS 绑定。

```sh
cargo test --locked --features workbuddy-lab,sample-demo
cargo test --locked --no-default-features --features dstack-tdx
python -m unittest discover -s deployment/phala -p test_prepare.py
```

本地测试使用受控上游和模拟证据，不代表真实服务账号验证或生产安全审计。内存状态的周期同步存在崩溃丢失窗口，详见运行文档。

许可证：MIT；依赖各自适用其许可证。
