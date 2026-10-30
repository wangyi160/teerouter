<a id="account-bound-vless-egress"></a>

# 按账号绑定的 VLESS 出口

**简体中文** | [English](VLESS.en.md)

通过所有者认证的账号同步通道，账号的 `proxy` 字段可接受 VLESS 分享链接。示例仅使用虚构凭据：

```json
{"proxy":"vless://12345678-1234-1234-1234-123456789abc@node.example:443?encryption=none&security=tls&type=xhttp&sni=node.example&path=%2Ftunnel&mode=stream-one"}
```

支持 TLS 配合 TCP 或 XHTTP（`auto`、`stream-one`、`stream-up`、`packet-up`）。可选字段包括 `sni`、`fp`（chrome/firefox/safari/edge/randomized）、`alpn`（h2/http/1.1）、XHTTP 的 `host`/`path`/`mode`，以及 TCP 的 `flow=xtls-rprx-vision`。只接受 UUID 身份与 `encryption=none`。REALITY、WS、gRPC、XHTTP extra JSON、不安全证书和未知/重复参数均被拒绝。IPv6 节点需要 DNS 名称形式的 `sni`。证书验证不可关闭；节点 SNI 对应的公共 CA 证书必须有效。

账号绑定作用于模型发现、推理/流式请求和凭据探测。Router 不执行 OAuth 续期。官方上游域名及其 HTTPS 证书检查仍由 Router 强制执行。VLESS 路由失败不回退到直连或系统代理。目标主机名通过 SOCKS 传给 VLESS，只有节点本身在本地解析。内层提供商 TLS 终止于提供商，而非 Xray。

Router 使用 `teerouter-egress`，这是嵌入固定版本 Xray-core 的私有 Go 辅助进程。Rust 按需启动一个进程，仅开放带认证的回环 SOCKS 监听器，密码随机生成且不对外公开；配置通道为 stdin/stdout，不是网络 API。UUID 和配置仅存在于内存和私有管道，不进入命令行参数或文件。辅助进程没有公开配置端点。

相同节点配置可被多个账号共享。活跃 HTTP 客户端持有路由；替换客户端时，旧路由等待已有流结束。每个 Router 最多有 64 条活跃/排空中的节点路由和 2048 个并发 SOCKS socket，同时受 runtime 的内存/进程限制。容量耗尽时拒绝准入。辅助进程退出使当前请求失败；下一个新建客户端可启动新的辅助进程。私有账号绑定通过所有者认证的同步协议重建；根检查点不含这些秘密。

在 `egress/` 中进行本地源码检查，使用 `go.mod` 指定的 Go 版本：

```sh
go build -mod=readonly .
go test ./...
```

测试覆盖本地 TLS/XHTTP、认证、失败时拒绝放行、流排空和取消。模块版本锁定于 `go.mod` 与 `go.sum`，依赖许可要求记录在仓库 `NOTICE` 中。辅助进程属于 TEE 信任策略覆盖的代码。
