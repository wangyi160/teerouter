# Phala / dstack TDX 部署

生产 dstackOS 承载一个 Rust router 容器；TLS 私钥与业务状态只保存在 router 内存中。独立管理服务和用户验证客户端运行在 TEE 外。无需 Kubernetes、CoCo 控制面或自建 Trustee。

## 构建

从仓库根目录执行：

```sh
docker build --platform linux/amd64 -f deployment/phala/Dockerfile -t teerouter:local .
```

默认仅启用 `dstack-tdx`，构建时运行 release 启动与恢复测试。最终镜像基于 scratch，仅包含 router、动态运行库和许可文件，没有 shell、SSH 或包管理器。构建上下文为源码白名单。

将镜像推送至自己控制的仓库并记录远程 **registry manifest digest**，然后生成 Compose：

```sh
python deployment/phala/prepare.py compose REGISTRY/OWNER/teerouter@sha256:ACTUAL_DIGEST --output dist/phala/compose.yaml
```

工具拒绝可变 tag。开发用 Workbuddy 和 sample 特性不应加入生产镜像。

## 部署

安装官方 Phala CLI 后登录。部署前选择支持 TDX 的 CPU 实例和已审核的生产 dstackOS；机器库存、价格和 CLI 参数以部署时查询为准。

```sh
phala login
phala instance-types
phala nodes
phala os-images --prod
phala deploy --help
```

通过 Phala 的加密环境变量注入 `TEELLM_ADMIN_KEY`，密钥至少 32 字符。使用生成的 Compose 和本目录 `pre-launch.sh`；只允许该环境变量，不开放额外容器、宿主目录、Docker socket 或启动脚本。部署会产生云费用。

需要审查完整 app-compose，而不只是 Compose YAML：生产 OS、镜像摘要、环境变量白名单、pre-launch、日志、调试、挂载、存储与 swap 等配置均属于信任边界。模板关闭容器日志、去除 capabilities、启用只读文件系统与 no-new-privileges，内存与内存加 swap 上限相同。不要把平台未显示的字段直接当作安全值。

对外只开放 8443，HTTP 管理端口 4000 仅绑定容器回环。guest 的 `/var/run/dstack.sock` 提供 quote；不挂载 Docker socket。

客户端使用 TLS passthrough 入口：`https://APP_ID-8443s.CLUSTER.phala.network`。TLS 在 router 内终止，网关不能替 router 终止该连接。

## 独立验证

1. 审核源码并构建，记录镜像 digest；保存最终实际部署的完整 app-compose。
2. 使用 `python deployment/phala/prepare.py hash APP_COMPOSE.json` 计算规范化启动配置摘要。
3. 从独立可信来源获得并审核 OS 参考测量值，结合 app-id、compose-hash、OS-image-hash、KMS 身份及完整事件序列建立客户端本地发布策略。证明与 TLS 绑定协议见 [TEE 安全边界](../../SECURITY-TEE.md)。验证客户端独立提供，不包含在此仓库。
4. 使用验证客户端检查 Intel 签名、吊销、TCB、非 debug、MRTD、RTMR 和新随机挑战与当前 TLS 证书的绑定。未知发布必须拒绝；不要仅从待验证实例自身生成允许表。
5. 验证管理通道后再导入账号和用户信息；用户客户端使用自己的策略独立验证。

项目没有为新部署预先批准任何 OS 或应用。客户端没有本地批准的发布策略时必须拒绝连接。固定某个 KMS 身份也不等于完整审计了 KMS 治理和平台运行权限。

## 初始化与恢复接口

外部管理服务在验证 router 身份和 TLS 绑定后，通过 `/admin/checkpoint`、`/admin/initialize`、`/admin/apply` 与 `/admin/checkpoint/ack` 初始化和持续同步。停止容器前保存最新快照，重启后根据新的 boot_id 恢复；详见 [运行文档](../../OPERATIONS.md) 和 [API 索引](../../docs/API.md)。管理系统的实现独立维护，不包含在此仓库。

拒答审计默认启用，无需独立审核密钥；LockLLM 不再调用。管理员通过用户接口查询拒答原文和元数据，通过 checkpoint 同步归档，不保存请求正文。恢复旧快照时仍保留旧审核历史。模型服务就绪仍要求可用上游与及时的管理同步。
