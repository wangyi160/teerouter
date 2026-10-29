# teerouter-verify

独立消费凭据校验器，支持 Windows / Linux。校验原始批次签名、流水摘要、顺序、金额汇总及衔接；可结合 Intel TDX 证明与独立指定的参考策略验证签名身份。

## 当前公开格式

支持 `teerouter.public-consumption-batch.v1`。`manifest` 与 `records_json` 按原始 UTF-8 字节验证，不得重新序列化。公开批次使用完整记录摘要，每批一个签名，不使用单条 Merkle 证明。用户和 key 代号按批次变化，拒答只保留类型。

没有 `records_json` 时只能验证签名与承诺的摘要，不能证明明细已通过核验。

## 验证输入与结果

输入为原始消费证据包和独立可信的 TDX 策略文件：

```sh
teerouter-verify --bundle batch.json --policy trusted-tdx-policy.json
```

成功退出码为 0，失败为 1。JSON 结果分别报告 `tee_verified`、`signature_verified`、`records_verified`、`inclusions_verified`。

`tee_verified` 只有在证据与本机独立认可的 OS、Router compose 和密钥提供方策略一致时才为 true。证据自身的公钥或“已验证”标签不能充当参考策略。`policy/deny-all.json` 不信任任何证据。

验证默认获取 Intel 当前证书、吊销与 TCB 材料，并使用本机当前时间。过期或撤销的历史证据可能失败；批次自报时间不是可信时间戳。

仅检查数学签名时：

```sh
teerouter-verify --bundle batch.json --signature-only
```

此模式始终返回 `tee_verified:false`，不证明 TEE 身份。`tests/sample-bundle.json` 使用公开测试种子，只是测试向量。

## 历史私有格式与限制

校验器保留对已签发私有批次的验证，包括带随机盐的 payload、Merkle 路径和历史空批次格式；这些不是当前 Router 的流水查询或上传接口。格式见 [消费批次协议](../docs/CONSUMPTION-PROOFS.md#historical-private-format)。

- 签名证明已给出字节的出处和完整性，不证明历史完整、没有被隐去或一定最新；`complete_history`、`latest_verified` 始终为 false。
- 输入可附上一批签名以验证衔接。前批采用另一公钥时，仍需独立验证其身份；`previous_tee_verified:false` 明确这一限制。
- Router 异常退出可能丢失未提交记录。签名和重启标识不能补造缺失流水。
- 消费事件的证明不覆盖外部余额变更，且不保证每次网络异常都有完整官方用量。

## 本地构建与测试

```sh
cargo build --release --locked --manifest-path verifier/Cargo.toml
cargo test --locked --manifest-path verifier/Cargo.toml
```

TDX 验证检查 OS/RTMR、完整事件重放、Intel 证书根与 TCB。测试向量来源、SHA-256 和许可证见 [测试证据](tests/fixtures/tdx/README.md)。
