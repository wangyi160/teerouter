<a id="tee-security-boundaries"></a>

# TEE 安全边界

**简体中文** | [English](SECURITY-TEE.en.md)

Router 实现 TLS 1.3、模型转发、dstack Intel TDX 与 Linux SNP 取证。本文说明代码所验证的身份、数据边界与证明限制。

<a id="keys-and-permissions"></a>

## 密钥与权限

TLS 私钥和每次启动的消费签名私钥在 Router 内生成，不提供导出接口。商户账号、access token 与代理秘密只在对应 runtime 中使用；根控制接口只能读取脱敏状态。管理员授权、商户控制密钥和用户签名密钥不能相互替代。

用户请求同时要求有效 Bearer 与客户端 Ed25519 签名。持有 Bearer 或管理员权限不能伪造用户签名；管理员仍可改变授权、余额和服务准入。该性质以运行中的 Router 实现与验证者批准的代码一致为前提。

可选账务日志使用本地 dstack `GetKey` 派生密钥逐行认证加密，不接受外部提供的日志密钥。日志不保存模型正文、上游账号凭据或 TLS 私钥。

<a id="channel-attestation"></a>

## 通道证明

随机挑战和当前 TLS 证书通过以下消息绑定到 REPORT_DATA：

```text
SHA512(UTF8("teellm:attested-channel:v1\0") || nonce32 || SHA256(certDER))
```

`dstack-tdx` 通过本地 guest 接口取得 TDX quote。验证器核验 Intel 证书链、吊销与 TCB、MRTD、RTMR0–2、完整 RTMR3 事件序列及 REPORT_DATA；证据与参考策略必须独立。网络提供的材料本身不能成为信任结论。

Linux SNP 路径通过 `/dev/sev-guest` 取证，并返回 CoCo SNP 结构。报告必须结合 AMD 证书链、签名、新鲜度、版本、TCB、DEBUG、VMPL 和独立参考值验证。当前独立消费凭据校验器支持的证据类型见其 [说明](verifier/README.md)。

指定取证接口不可用时返回 `attestation_unavailable`，不回退到软件声明。`sample-demo` 是显式测试特性，证据带 sample 标记，严格验证器拒绝。

硬件签名不能单独证明任意代码或文件可信。它只有与完整测量链、已批准参考值和当前连接绑定共同验证，才支持对应的信任结论。本地单元测试不构成真实硬件证明。

<a id="content-and-upstream-services"></a>

## 内容与上游

提供商域名固定在代码中。账号显式绑定代理，失败不回退直连；官方 HTTPS 证书仍由 Router 校验。官方目录与能力来自经过认证的上游响应，控制输入只能收紧模型权限，不能注入官方目录证据。

原生转发保留消息、instructions、tools、附件与生成参数；路由标识解析不改变实际请求的模型语义。配置不能注入生成正文、重写思考参数或替换协议。`store` 沿用调用者值，Router 不承诺上游内部的数据保留行为。

明确的 refusal/content_filter 信号用于拒答分类。公开批次只包含类别和计费元数据，不包含输入、回答、拒答原文或工具内容。分类不是对用户行为的法律或合规判断。

<a id="state-integrity-and-limitations"></a>

## 状态完整性与限制

根检查点包含公开配置、用户授权、商户公开登记、累计用量和 relay 状态，不包含商户秘密。官方目录、健康状态和会话绑定在新进程中重新建立。读取检查点不结算消费；结算入口单独验证版本、启动标识与已上传边界。

本地日志校验每行及完整行集合，拒绝未经认证的替换、删除和插入；已冻结批次保持原始签名，新批次使用当前进程密钥。恢复同时核对确认边界。该机制不能阻止整个日志副本回滚或删除，不能恢复未提交的在途记录。

无日志模式的未同步用量可能丢失。日志写入失败会关闭新的准入，但此前尚未提交的尾部仍不受恢复保证保护。消费签名证明出处与完整性，不能证明历史完整、未被隐去或必然最新。详情见 [消费批次](docs/CONSUMPTION-PROOFS.md) 与 [本地日志](docs/ACCOUNTING-JOURNAL.md)。
