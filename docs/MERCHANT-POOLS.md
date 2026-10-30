<a id="merchant-pool-state"></a>

# 商户账号池状态

**简体中文** | [English](MERCHANT-POOLS.en.md)

经过所有者认证的 `PUT /merchant/accounts/sync` 创建或更新账号绑定。新的非空池可携带初始 `new_pool_pricing`。现有池策略与倍率保持不变；公开策略更新使用管理员认证的 `PUT /admin/merchants/{mid}/config`。

没有账号的池及其 `pool_pricing` 项不会发布。移除最后一个账号会撤下该池；停用的账号仍计作账号。其他商户不受影响。空草稿不是已运行的账号池。

`POST /merchant/accounts/test` 可在池尚不存在时测试账号。请求包含 `pool`、`proxy`、仅含访问凭据的 `credential`（或 `api_key`），以及：

```json
{"draft_pool":{"provider":"openai","allowed_models":["model-id"]}}
```

`allowed_models:null` 允许所有官方可用的文本模型；`[]` 不允许任何模型。只有当前商户 runtime 中不存在该池时才接受草稿。已有池使用当前提供商和模型策略，拒绝草稿覆盖。官方端点、代理、授权及探测限制照常生效。

测试不会创建账号池、持久化凭据、续期令牌或生成用户消费记录。账号同步与账号测试是两个独立操作。
