# teerouter-verify

0.1.2 支持 `teerouter.public-consumption-batch.v1`：验证公开流水的签名、原始
`records_json` 摘要、顺序、金额汇总和批次衔接。用户及 key 代号按批次变化，拒答只保留类型。
公开批次没有 Merkle 单条证明；单条下载包含所在批次的全部公开记录。
不带 `records_json` 的归档摘要只能核验签名和承诺的哈希，不能核验明细。
下文关于私有记录、Merkle 证明和空批次的说明适用于旧版私有流水协议。

独立的开源消费流水校验器。无需安装 teeproxy、teemerchant 或运行 Router。
支持 Windows / Linux，通过本机代码验证 Ed25519 签名、Merkle 包含证明、
Intel TDX 硬件证明和独立指定的 Router 发布策略。

## 使用

在 Portal 的 `/portal/receipts` 下载批次凭据；登录后在
`/portal/receipts/mine` 下载自己的单笔消费凭据。

```sh
teerouter-verify --bundle batch.json --policy trusted-tdx-policy.json
```

成功退出码为 0；失败为 1。结果为 JSON，分别报告 `tee_verified`、
`signature_verified`、`records_verified`、`inclusions_verified`。
只有 TEE 证明与本机预先认可的 OS、Router compose 和密钥提供方策略一致，
`tee_verified` 才为 true。服务器的证明、Portal 的“已核验”标签和凭据里的
公钥都不能自行充当信任策略。`policy/deny-all.json` 故意不信任任何发布。

默认从 Intel 官方服务获取证书、吊销和 TCB 材料，在当前本机时间验证。
历史证明遇到过期或撤销会验证失败，不能把凭据自报的时间当作可信验证时间。
批次时间是 Router 报告的时间，不是外部可信时间戳。

仅供开发或已有独立公钥信任关系时，可以检查数学签名：

```sh
teerouter-verify --bundle batch.json --signature-only
```

此模式始终返回 `tee_verified:false`，不能据此宣称可信 TEE 存证。
`tests/sample-bundle.json` 使用公开测试种子，只是测试向量，没有 TEE 证明。

## 范围

- 存证仅包括 Router 消费记录及其请求时用量、价格、倍率和金额。
- 充值、退款、提现和人工结算由管理平台及支付渠道记录，不由此证明背书。
- 批次原文和记录原文均按 UTF-8 **精确字节**签名/哈希，不能重新排版后验证。
- 公共包没有个人消费明细。单笔包包含该笔原文、随机盐和兄弟节点摘要，
  无需公开同批其他用户或商户的流水。
- 空批次表示本次同步没有新流水。自动同步约 5 分钟一次，手动同步可立即出批。
- Router 异常退出允许丢失未保存的记录；重启通过新公钥、boot ID 和
  `continuity:restart` 明示。从最后持久化批次恢复，不伪造缺失流水。
- 签名证明出处和完整性，不证明所有流水都存在、Portal 没有隐藏批次、
  当前批次一定最新，或支付已实际完成。`complete_history` 和
  `latest_verified` 始终为 false。
- 包可附上一批签名，工具核验衔接和上一批数学签名；上一批若使用另一
  公钥，仍须独立验证其证据包。`previous_tee_verified:false` 明确这一边界。

## 构建与测试

```sh
cargo build --release --locked --manifest-path verifier/Cargo.toml
cargo test --locked --manifest-path verifier/Cargo.toml
```

Intel TDX 核验代码复用同一项目体系的公开实现，保留精确 OS/RTMR、
完整启动事件重放、Intel 生产根与严格 TCB 校验；没有捆绑桌面代理程序。
测试中的 Intel 已签名向量及其来源、许可证见 `tests/fixtures/tdx/`。

协议细节见 [CONSUMPTION-PROOFS.md](../docs/CONSUMPTION-PROOFS.md)。
