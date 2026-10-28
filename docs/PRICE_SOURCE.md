# LiteLLM 价格数据源

Router 可从公开 GitHub JSON 自动维护 OpenAI、Anthropic、xAI 的直接服务文本模型价格。默认地址是：

`https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json`

配置通过**平台管理员**认证通道设置；普通用户和商户不能修改。配置、更新状态和价格簿一起进入加密 checkpoint。新部署默认关闭，管理员启用后首次拉取，之后每 18000 秒（5 小时）检查一次（后台检查间隔最多 30 秒）。重启不会重置上次更新时间；已经到期时恢复服务后更新。

## 配置和手动更新

先 `GET /admin/pricing/source`，再带配置版本提交：

```http
PUT /admin/pricing/source
Content-Type: application/json

{"expected_revision":0,"enabled":true,"url":"https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json"}
```

`enabled` 控制定时任务；关闭自动任务仍可手动检查：

```http
POST /admin/pricing/refresh
Content-Type: application/json

{}
```

支持 `github.com/{owner}/{repo}/blob/{ref}/{path}.json` 和 `raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}.json`。ref 可为不含斜线的分支、tag 或 commit SHA；固定 SHA 适合审计复现，跟随 main 适合持续更新。不接受私有仓库凭证、URL 用户名、参数、fragment 或非标准端口。

Router 使用 GitHub Contents / Git Blobs API 读取配置文件，完整 TLS 校验，不跟随重定向，不附加平台、用户或商户密钥。一次检查最长约 120 秒，响应最大 16 MiB。手动刷新立即返回 `accepted: true, running: true`；下载在后台进行，不占用管理连接。随后轮询 `GET /admin/pricing/source`，以 `running: false` 和 `last_error` 判断结果。并发更新返回 `409 pricing_sync_running`，配置版本冲突返回 `409 pricing_source_conflict`。下载或校验失败记为 `pricing_fetch_failed` 或 `invalid_price_source`，保留旧价格；下一次自动检查在上次尝试的 5 小时后，管理员可随时重试。

状态包含 `last_attempt`、`last_success`、`last_error`、`next_update_at`、`running`、源文件 SHA-256、Git blob SHA 和最近 32 次成功检查的记录。`history.changed` 表示是否改变价格版本。完整账务历史由管理平台保存，请求回执冻结实际使用的报价、来源、价格版本和商户倍率。

## 导入范围与计费

ChatGPT 先取 `chatgpt/` 前缀、`litellm_provider: chatgpt` 的文本目录，再按完全相同的模型 ID 关联 `litellm_provider: openai` 的直接 API 标价；不导入 OpenAI API 全集。没有同名官方报价时列入管理状态的 `unpriced_models`，不猜测相近型号价格、不取第三方转售价格，也不填零。缺失整个 ChatGPT 目录时拒绝本次更新。

Claude 和 Grok 只取 `litellm_provider: anthropic` / `xai` 的对应 `claude-` / `grok-` 模型。三家都要求 `mode` 为 `chat` / `responses`；Azure、Bedrock、OpenRouter 等中转条目、通用微调模板、容器、专用音频、视频与生图条目不导入。文本模型可以具有理解图片的能力，这不代表开放生图功能。目录中的旧型号按数据源保留；价格目录不代表账号拥有该模型的调用权限。

导入输入、输出、缓存读取、缓存写入、Claude 一小时缓存、长上下文和完整的 priority/flex 报价。Claude 明确提供的 US / fast 倍率应用到 token 价格及其组合，遵循 [LiteLLM Anthropic 计算器](https://github.com/BerriAI/litellm/blob/main/litellm/llms/anthropic/cost_calculation.py)。xAI 长上下文边界保持包含阈值；其他提供商使用大于阈值。

源单位 USD/token，转换到 micro-USD/百万 token 时乘 10^12，全程十进制整数运算；不能精确表示或负价格导致整次更新失败。缺失缓存单价保持缺失，不当成免费。可选服务档位缺少输入、输出或必要长上下文价格时不开放该档位，避免套用普通档低收费用。Batch、ultrafast、decisions、托管工具、音频等当前未开放的收费维度不映射成普通档；请求按现有计费能力校验。

三家基础价格必须同时有效才能整体提交。模型价格和来源相同不增加价格版本，最近成功检查时间仍更新。更新只影响后续请求；正在执行的请求继续使用原报价。启用源管理后，`PUT /admin/pricing` 不能覆盖三家价格，只允许价格源整体更新。

teeproxy 价格页从管理平台读取缓存的已生效价格，只展示当前可用账号池已开放的模型。Router 不提供用户价格查询；每 5 小时更新及管理员手动更新的价格通过已有检查点同步给管理平台，未变化的价格不因流水同步反复重写。提供商的 `source` 明确记录配置的 GitHub 数据源；LiteLLM 是第三方维护的价格汇总，自动同步保证与所选数据源一致，并不独立保证其与服务商官网没有更新延迟。源文件与计费实现的角色不同：更换数据源地址不会替换 Router 计算器。
