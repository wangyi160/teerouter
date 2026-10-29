# teerouter

运行在 TEE 中的 Rust 大模型 Router。本仓库文档描述 Router 的代码结构、HTTP 接口、原生转发、认证、计费、状态同步和验证方法。

## 核心能力

- 支持 OpenAI / ChatGPT、Claude / Anthropic、Grok / xAI；通过各账号绑定的代理访问固定的官方 HTTPS 域名。
- 原生转发 Chat Completions、Responses、Messages，保留调用者的生成参数。解析路由模型标识后，只将对应的官方模型 ID 发送上游。
- 官方模型目录按提供商共享；账号凭证、健康状态、限额、池级开放名单和用户权限分别校验。目录失效时拒绝依赖该目录的新请求。
- 所有上游账号归属隔离的商户 runtime。用户请求同时校验 Bearer 授权与客户端 Ed25519 签名；账号秘密仅经所有者认证的商户通道接收。
- 固定池与自动路由支持倍率上限、费率和代理延迟优先，保留会话亲和性。用户所有 API key 共享并发门，余额不足或队列容量耗尽时停止新请求。
- 从原生响应读取用量，按提供商规则和请求开始时冻结的报价生成费用；中断时按已观察的发送进度与用量确定结果。
- 消费记录组成 Ed25519 签名批次，上传确认与结算确认分别处理。可选认证加密日志恢复已提交的账务记录。
- TLS 1.3、挑战绑定的 dstack TDX / Linux SNP 取证与模型转发位于同一进程。另提供独立的消费凭据校验器。

`GET /admin/public-catalog` 返回 Router 当前池、模型、价格、容量以及可用账号的脱敏摘要。它是只读控制接口，不触发目录刷新、不建立客户关系；不返回凭证、代理地址或原始错误。接口权限和请求格式见 [API](docs/API.md)。

## 代码与示例

| 路径 | 内容 |
| --- | --- |
| `src/`、`tests/` | Router 实现与测试 |
| `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml` | Rust 构建与锁定依赖 |
| `config*.example.toml`、`provision*.example.json` | 内部引擎的协议测试夹具 |
| `protocol-update.example.json`、`restrictions-update.example.json` | 配置结构示例 |
| `egress/` | VLESS 出口辅助进程源码 |
| `verifier/` | 独立消费凭据校验器 |
| `docs/` | Router 接口、协议与数据边界 |

TOML 和本地凭据文件的装配仅在 `cfg(test)` 夹具中使用。根 runtime 的初始化配置不接受上游账号池；商户账号与公开策略使用不同认证通道，见 [商户协议](docs/MERCHANTS.md)。

## 本地构建与测试

crate 和可执行文件名为 `teellm`。

```sh
cargo check --locked --lib --bins --no-default-features
cargo check --locked --lib --bins --no-default-features --features dstack-tdx
cargo test --locked --all-targets --features sample-demo
```

`sample-demo` 默认关闭，仅用于受控测试；它不生成可被严格验证器接受的硬件证据。本地测试使用模拟上游，不证明真实账号可用，也不替代真实硬件证明验证。日志只保护已经提交的记录，见 [流水日志](docs/ACCOUNTING-JOURNAL.md)。

## 文档索引

- [代码结构](ARCHITECTURE.md)、[状态与同步协议](OPERATIONS.md)、[HTTP API](docs/API.md)、[TEE 安全边界](SECURITY-TEE.md)。
- [用户请求签名](docs/USER-AUTH.md)、[商户认证与配置](docs/MERCHANTS.md)、[账号池](docs/MERCHANT-POOLS.md)、[VLESS 出口](docs/VLESS.md)。
- [计费](docs/BILLING.md)、[中断计费](docs/INTERRUPTED-BILLING.md)、[价格源](docs/PRICE_SOURCE.md)、[商户记账边界](docs/MERCHANT-LEDGER.md)。
- [签名消费批次](docs/CONSUMPTION-PROOFS.md)、[本地流水日志](docs/ACCOUNTING-JOURNAL.md)、[独立校验器](verifier/README.md)。

代码包含 LiteLLM 选定模块的移植及新建 Rust 集成。来源与修改见 [NOTICE](NOTICE)、[UPSTREAM.json](UPSTREAM.json)。许可证为 MIT；依赖各自适用其许可证。
