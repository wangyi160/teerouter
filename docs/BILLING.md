# 按模型提供商计算费用（billing v2）

`/admin/status` 的 `provider_billing_version: 2` 表示 Router 在进程内生成逐请求计费结果，逐次上游尝试分别保留 v1 计算明细，再汇总并只舍入一次。异常请求按实际发送进度处理。生产 TEE 构建中，计算代码和计费事件与 Router 一起受镜像度量约束。外部管理平台保存余额、归档事件并入账。计费规则由模型提供商决定，与提供账号的商户无关。

## 计费基准与精度

余额持久保存在管理平台。Router 只缓存上次同步余额和本轮消费，按两者之差检查新请求；请求结束时累加已计算费用，不修改余额。管理平台定期拉取记录，在事务中扣款后通过 checkpoint ACK 更新 Router 余额并清除已结算消费。同步期间完成的新请求保留到下轮；待确认的费用不猜算，管理端确认入账后通过后续余额同步体现。协议和恢复步骤见 [运行文档](../OPERATIONS.md)。

价格数据可由 Router 按日同步 LiteLLM GitHub JSON，并由平台管理员手动刷新；配置、更新状态、版本及导入范围见 [价格数据源](PRICE_SOURCE.md)。

基准是官方公开 API 价目表；用户费用等于基准费用乘账号池倍率。订阅账号实际支出、赠金、私有折扣和税款不属于此基准。`merchant_pricing` 在请求开始时冻结费率、规则、版本、来源和倍率，后续调价不追溯修改已经开始的请求。

Token 单价仍为整数 micro-USD / 百万 Token。内部用 `u128` 按 10^-12 USD 累计各分项，乘倍率后只向上舍入一次至 micro-USD。事件同时保留舍入前的精确十进制整数字符串；管理端可核对分项、合计、倍率及舍入。该舍入约定沿用平台现有精度，不声称与服务商月账单的舍入、折扣或税额完全相同。

## 三种计算器

* `chatgpt_v1`：OpenAI 输入总量包含缓存部分；普通输入减去缓存读写后分别计价。推理 Token 属于输出明细，不能再加一次。
* `claude_v1`：Anthropic `input_tokens` 不含缓存读写；普通输入按原值计费，缓存读、5 分钟写、1 小时写分别计价。长上下文判断使用包含缓存的总输入。支持有明确报价与用量计数的官方 web search。
* `grok_v1`：兼容旧 Chat 分列完成和推理 Token、新 Responses 将推理包含在输出内的格式；用 `total_tokens` 校验并判定是否需要增加推理量，无法确定则待核对。官方 `cost_in_usd_ticks` 原样记录为实际成本参考，不替代公开标价基准。
* 显式测试构建中的 `workbuddy_v1` 使用测试价格簿，不能冒充官方模型价格。

采集直接读取各家原生响应；Router 不转换 Agent 的生成协议，也不修改缓存、TTL 或 usage 请求参数。流中的累计用量覆盖前值，不能逐帧累加。只保留白名单数字和枚举，不保存请求文本、正常回答、工具参数或工具结果。

## 报价扩展

用户价格页由管理平台提供。管理平台通过已有差量检查点接收 `official_pricing` 和 `pricing_source`，缓存 Router 实际计费使用的版本，并按当前可用账号池的开放模型筛选；页面查询不会再调用 Router。Router 保留计费价格更新、管理员控制和检查点同步，不再提供 `/v1/pricing` 用户查询接口。

`/admin/pricing` 的模型行新增可选 `billing_rules`；旧价格簿仍可加载。它随报价进入 `merchant_pricing` 和模型目录，修改时必须提升官方价格版本。

```json
{
  "billing_rules": {
    "variants": [
      {
        "service_tier": "priority",
        "speed": "standard",
        "inference_geo": "global",
        "rates": {"input": 4000000, "output": 20000000, "cache_read": 200000},
        "long_context": null
      }
    ],
    "tools": {},
    "long_context_inclusive": false
  }
}
```

上述数字仅展示格式，不能作为任意模型的有效报价。每个 variant 是档位、速度、地域的完整组合，包含组合后的明确单价，不自动猜测倍率叠加。标准组合 `default / standard / global` 使用模型原有费率。按上游返回的实际档位选择报价；上游从请求的加速档回落到标准档时使用标准价。`auto` 缺少实际档位则不结算。

Anthropic 的 `standard`、`standard_only` 归一为 `default`；OpenAI/xAI 的 `fast` 归一为 `priority`。Anthropic `speed: fast`、`inference_geo: us` 是独立维度。兼容配置不能改写这些计费维度。

默认长上下文条件保持 `input > above_input_tokens`。xAI 官方使用包含边界的阈值时，设置 `long_context_inclusive: true`，条件为 `input >= above_input_tokens`；例如 Grok 4.7 的 200,000 Token 边界。超过边界后整次请求使用对应档位，不只是超出部分。

Anthropic 搜索每次调用的 micro-USD 价格位于 `tools.web_search`，计数来自原始 `usage.server_tool_use.web_search_requests`。报价缺失则请求前拒绝，启用了工具但未返回调用计数则待核对。

## 事件和外部入账

平台和对应商户收到相同的 `event.billing`，外层事件绑定请求 ID、用户、商户、池、模型和请求时间。

* `state: calculated`：包含 `version`、`calculator`、`provider`、`basis: official_list_price`、`currency: USD`、价格版本、倍率、实际维度、原始计费数字、`items`、`official_cost_picousd`、`charge_numerator`、`charge_denominator`、`rounding`、`official_cost_microusd`、`charge_microusd`。
* `state: pending`：包含明确的 `reason`，不包含可扣款金额。不能转换成免费或退回旧公式猜算。
* `items` 的 `amount_picousd` 为字符串；Token 项单位 `million_tokens`，工具项单位 `call`。`charge_numerator / charge_denominator` 为舍入前的 micro-USD，分母固定 `1000000000000`。

管理端归档 Router 发布的签名批次，校验批次签名、金额、价格版本和倍率后原子入账并确认归档；重试不重复扣费。管理端连接 Router 不需要执行客户端的 TEE 核验流程。旧版本历史事件没有 `billing` 时仍按历史逻辑处理。新事件存在 `billing` 但结果待核对、版本未知或不合法时，禁止退回历史计费公式。

独立验证者可核验批次签名、摘要及签名公钥的 TEE 绑定，见[消费凭据](CONSUMPTION-PROOFS.md)。运行中的流水日志和恢复边界见[本地记账日志](ACCOUNTING-JOURNAL.md)。签名证明记录来源与完整性，不代表所有网络异常都有完整的官方用量。

## 当前边界

* 缺少请求档位的报价、已知不支持的收费功能，返回 `400 billing_unsupported`，不发送模型请求。
* 正常成功但原始用量不完整、数字不合法、缓存拆分矛盾或计价溢出，保留明确待核对原因。失败、中断及多个上游尝试按[异常请求计费规则](INTERRUPTED-BILLING.md)分别处理：未完整发送或明确被拒绝时不收费；正文已写出后等待回复时中断，估算输入；收到部分输出后中断，使用官方计数及已观察文本估算。估算明确标记，不冒充官方用量。
* 非标准档位、速度和区域必须由上游响应确认；未回报实际维度则待核对，避免降档后仍按高价扣费。
* OpenAI 托管搜索、文件搜索、代码执行、图像生成及音频/预测输出的专用规则尚未覆盖，相关请求拒绝；普通客户端 function/custom 工具继续支持。
* xAI 托管工具 REST 用量只有总次数而缺少可靠分类时，不能把不同单价的工具视为同价，因此暂时拒绝托管工具；保留实际成本字段供对账。
* Claude 多迭代/自动压缩涉及独立上下文边界，目前标记 `iteration_pricing_unavailable`；不把所有迭代合并后猜测阶梯价。
* 报价计算与报价采集独立。本次增加计算器和价格规则协议，不自动抓取官方网页；管理员仍需维护完整、有效且可核对的官方价格簿。

官方依据：[OpenAI 定价](https://developers.openai.com/api/docs/pricing)、[Claude 定价](https://platform.claude.com/docs/en/about-claude/pricing)、[Claude 缓存用量](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)、[xAI 定价](https://docs.x.ai/developers/pricing)、[xAI Chat 用量](https://docs.x.ai/developers/rest-api-reference/inference/chat-completions)、[xAI Responses 用量](https://docs.x.ai/developers/rest-api-reference/inference/responses)、[xAI 实际费用字段](https://docs.x.ai/developers/cost-tracking)。规则核对日期：2026-10-14。
