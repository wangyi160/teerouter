<a id="signed-consumption-batches"></a>

# 签名消费批次

**简体中文** | [English](CONSUMPTION-PROOFS.en.md)

Router 为消费记录生成可验证证据。签名认证已承诺的字节，不证明历史完整、状态最新或外部提供的余额正确。

<a id="public-relay-protocol"></a>

## 公开上传协议

`teerouter.public-consumption-batch.v1` 将签名批次发布到 HTTPS 凭据端点。`PUT /admin/receipts/relay` 需要管理员认证，接受 `url`（HTTPS 源地址）、`upload_token`、`interval_seconds`（默认 300），以及 `user_ids`（用户 ID 到私有随机 32 字节十六进制身份的映射）。

秘密和私有身份不进入公开批次或状态。启用后，目标地址和已有私有身份不可更改；上传令牌可轮换。`GET /admin/receipts/relay` 返回脱敏状态，`POST /admin/receipts/relay/flush {}` 请求立即上传。

每个非空批次使用新的公开 32 字节随机数：

```text
user_hash = SHA256(private_user_id_bytes || batch_nonce_bytes)
key_hash  = SHA256(UTF8("teerouter:receipt-key-id:v1\0")
                   || private_user_id_bytes || UTF8(key_id) || 0x00
                   || batch_nonce_bytes)
```

公开记录包含序号、轮换化名、商户/池/模型、状态、时间、用量、报价和计算费用。拒答仅含结构化类型；不含正文、拒答原文、凭据、用户名或持久用户/key ID。时间与流量特征仍可能关联记录，轮换标识不保证匿名。

每个封装包含原始 UTF-8 JSON 字符串 `manifest` 和 `records_json`，以及十六进制 `signature` 和 `batch_hash`。manifest 承诺 schema、算法、实例/启动/公钥、批次号、前批哈希、创建时间、序号范围、批次随机数、记录数、记录摘要和 micro-USD 总额。

```text
message        = UTF8("teerouter:public-consumption-batch:v1\0") || UTF8(manifest)
signature      = Ed25519.sign(process_private_key, message)
batch_hash     = SHA256(message)
records_sha256 = SHA256(UTF8(records_json))
```

每批一个签名，而非每条记录一个。公开记录不使用 Merkle 包含证明；核验记录字节需要原始完整 `records_json`，独立封装只能证明签名和承诺的摘要。

<a id="upload-and-settlement-boundaries"></a>

## 上传与结算边界

Router 使用上传 Bearer，向配置的凭据端点发送 `PUT /v1/streams/{instance_id}/batches/{batch_no}`。有效存储 ACK 必须包含 `durable:true`、匹配的 `batch_hash`、值为 `"pending"` 或 `"archived"` 的 `detail_state`，以及 `settlement_acknowledged:false`。

批次上限为 512 条且有字节大小限制。重试保留完全相同的已冻结签名字节。上传失败结束本轮，在下一 `interval_seconds` 周期重试，没有独立的 30 秒重试循环。

存储 ACK 释放对应原始明细，但不改变余额或清除消费。按用户/批次汇总的消费保留到结算。有容量上限的队列会拒绝新准入，不静默逐出已接受事件。

`POST /admin/receipts/relay/settle` 接受当前 `boot_id`、递增 `revision`、已上传的 `through` 边界，以及 `balances` 或 `balances_patch` 中恰好一项。Router 原子应用余额，仅清除截至该边界的消费，保留更晚请求。相同重试幂等；旧版本、冲突重试和未知边界被拒绝。

恢复时校验检查点边界与余额；启用本地日志时再对齐已提交记录。新批次使用新进程密钥和启动 ID，保留的冻结批次保持原签名。未成功持久化的未上传尾部可能丢失。仅凭签名或日志都不能证明历史未被隐去或回滚。见[状态同步](../OPERATIONS.md)与[账务日志](ACCOUNTING-JOURNAL.md)。

<a id="checkpoints-and-historical-evidence"></a>

## 检查点与历史证据

`POST /admin/checkpoint/diff` 只接受 `boot_id` 和 `base_id`，已移除的 `sign_receipts` 选项返回 400。检查点不创建私有批次，也不等待私有 ACK；它不是消费明细传输通道。

已有 relay 日志可能含不再生效的 `receipt_head:null` 元数据，读取时接受它且不改变事件或余额。非 null 的私有 head 会被明确拒绝；历史凭据行不会被静默丢弃。

独立校验器仍接受已经签发的私有证明包，用于验证历史原始字节，而非提供现行 Router 查询或私有检查点导入路径。

<a id="historical-private-format"></a>

### 历史私有格式

固定大小字节使用小写十六进制；金额字段不使用浮点数。

```text
message    = UTF8("teerouter:consumption-batch:v1\0") || UTF8(manifest)
signature  = Ed25519.sign(process_private_key, message)
batch_hash = SHA256(message)
leaf       = SHA256(0x00 || random_32_byte_salt || UTF8(record.payload))
parent     = SHA256(0x01 || left_32_bytes || right_32_bytes)
empty_root = SHA256(empty_bytes)
```

记录按 `seq` 排序；树的每一层若最后一个节点无配对，则复制该节点。包含证明提供叶子索引和自底向上的兄弟哈希；签名记录数决定深度与方向，记录序号必须等于 `sequence_before + index + 1`。历史 manifest 承诺范围、数量、已计价数量、计算总额、实例/启动及前批哈希/启动标识。

历史包包含 `batch` 和 `identity`；可选 `inclusions` 包含私有 `payload`、`salt`、`index` 和 `siblings`。这些字段不描述现行公开批次。已经签名的历史字节不重新格式化。

<a id="signing-key-evidence"></a>

## 签名密钥证明

`POST /admin/receipts/evidence {"nonce":"<新的32字节十六进制随机数>"}` 返回进程公钥、启动 ID，以及绑定下式的硬件证明：

```text
REPORT_DATA = SHA512(UTF8("teerouter:receipt-key:v1\0") || nonce_32_bytes
                     || public_key_32_bytes || boot_id_16_bytes)
```

该接口不为调用者提供的财务摘要背书。硬件证据使用本地 guest 接口取得；失败不会调用远程证明服务或软件回退。TLS 通道绑定仍为独立机制，保持不变。普通批次只需本地 Ed25519 签名。

[独立校验器](../verifier/README.md) 使用独立选定的策略验证证据。有效签名本身不建立硬件信任；验证消费证据无需公开整个检查点或账号秘密。
