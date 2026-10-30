<a id="litellm-价格数据源"></a>

# LiteLLM price data source

[简体中文](PRICE_SOURCE.md) | **English**

Router can automatically maintain prices for direct-service text models from OpenAI, Anthropic and xAI using a public GitHub JSON file. Default URL:

`https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json`

Configuration uses administrator-authenticated Router control endpoints; ordinary users and merchants cannot change it. Configuration, update status and the price book enter checkpoints together. Disabled by default, the feature fetches on enablement and then checks every 18000 seconds (five hours), with background checks at most 30 seconds apart. Restarting preserves the last-update time; overdue checks run after service recovery.

<a id="配置和手动更新"></a>

## Configuration and manual refresh

First read `GET /admin/pricing/source`, then submit the configuration revision:

```http
PUT /admin/pricing/source
Content-Type: application/json

{"expected_revision":0,"enabled":true,"url":"https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json"}
```

`enabled` controls scheduled checks. Manual checks remain possible when scheduling is disabled:

```http
POST /admin/pricing/refresh
Content-Type: application/json

{}
```

Supported URLs are `github.com/{owner}/{repo}/blob/{ref}/{path}.json` and `raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}.json`. A ref can be a branch without slashes, a tag or a commit SHA. A pinned SHA supports reproducible audits; following main supports ongoing updates. Private-repository credentials, URL usernames, queries, fragments and nonstandard ports are rejected.

Router reads the configured file through GitHub Contents / Git Blobs APIs with full TLS verification and no redirects. It does not attach control, user or merchant credentials. A check takes at most approximately 120 seconds and accepts a response of at most 16 MiB. Manual refresh immediately returns `accepted: true, running: true` and downloads in the background. Poll `GET /admin/pricing/source`; `running: false` and `last_error` describe completion. Concurrent updates return `409 pricing_sync_running`; revision conflicts return `409 pricing_source_conflict`. Download or validation failures report `pricing_fetch_failed` or `invalid_price_source` and retain old prices. The next automatic check is five hours after the last attempt; administrators can retry earlier.

Status includes `last_attempt`, `last_success`, `last_error`, `next_update_at`, `running`, source-file SHA-256, Git blob SHA and the latest 32 successful checks. `history.changed` indicates whether the pricing revision changed. Request receipts freeze the actual quote, source, revision and merchant multiplier.

<a id="导入范围与计费"></a>

## Import scope and billing

For ChatGPT, the importer first selects text-catalog entries prefixed `chatgpt/` with `litellm_provider: chatgpt`, then joins exactly matching model IDs to direct API list prices from `litellm_provider: openai`. It does not import the entire OpenAI API catalog. Missing exact-name quotes appear under `unpriced_models`; no similar-model price, third-party resale price or zero is substituted. A missing entire ChatGPT catalog rejects the update.

Claude and Grok select only corresponding `claude-` / `grok-` models from `litellm_provider: anthropic` / `xai`. All three require `mode` to be `chat` or `responses`. Azure, Bedrock, OpenRouter and other intermediary entries, generic fine-tuning templates, containers, dedicated audio, video and image-generation entries are excluded. Text models may understand images; that does not enable image generation. Older models remain according to the data source. A price catalog does not grant an account permission to call a model.

Imports cover input, output, cache reads/writes, Claude one-hour cache, long context and complete priority/flex quotes. Explicit Claude US / fast multipliers apply to token rates and their combinations, following the [LiteLLM Anthropic calculator](https://github.com/BerriAI/litellm/blob/main/litellm/llms/anthropic/cost_calculation.py). xAI long-context thresholds remain inclusive; other providers use strictly greater-than thresholds.

Source units are USD/token. Conversion to micro-USD per million tokens multiplies by 10^12 using decimal integer arithmetic throughout. Negative or inexactly representable prices reject the entire update. Missing cache prices remain missing, not free. Optional tiers without input, output or required long-context prices are not enabled, avoiding undercharging with standard rates. Unsupported billing dimensions such as Batch, ultrafast, decisions, hosted tools and audio are not mapped onto standard service; requests are checked against current billing capabilities.

All three providers' base prices must be valid for an atomic commit. Unchanged model prices and sources do not increment the pricing revision, but still update the last-successful-check time. Updates affect subsequent requests only; in-flight requests retain their frozen quotes. Once source management is enabled, `PUT /admin/pricing` cannot override these providers; only complete source updates are allowed.

Effective prices are exported through control endpoints and checkpoints. `/admin/public-catalog` quotes models allowed by each pool; Router has no user-facing price-query endpoint. Unchanged refreshes and receipt synchronization do not increment pricing revisions. Each provider's `source` explicitly identifies the configured GitHub data source. LiteLLM is a third-party price aggregation; synchronization ensures consistency with the selected source, not independent assurance that it has no lag behind official prices. Data collection and billing implementation are distinct: changing the source URL does not replace Router's calculators.
