<a id="按模型提供商计算费用billing-v2"></a>

# Provider-specific billing (billing v2)

[简体中文](BILLING.md) | **English**

`provider_billing_version: 2` in `/admin/status` indicates that Router calculates per-request billing in-process. Each upstream attempt retains its v1 calculation details; attempts are aggregated and rounded only once. Abnormal requests are handled according to actual transmission progress. Calculation code and accounting events remain within Router. Billing rules follow the model provider, independently of the merchant supplying the account.

<a id="计费基准与精度"></a>

## Billing basis and precision

Router retains the last synchronized balance and unsettled consumption, checking their difference before new requests. On completion it accumulates calculated charges without directly changing the synchronized balance. Upload ACKs release details only. `/admin/receipts/relay/settle` atomically updates balances and clears consumption within the acknowledged boundary, preserving later completions. Reading checkpoints does not settle charges. See [State synchronization](../OPERATIONS.en.md).

Router can check LiteLLM's GitHub JSON every five hours, with manual refresh also available. See [Price sources](PRICE_SOURCE.en.md) for configuration, update status, revisions and import scope.

The basis is the official public API list price; user charges equal the base cost multiplied by the pool multiplier. Subscription-account expenditure, credits, private discounts and taxes are outside this basis. `merchant_pricing` freezes rates, rules, versions, sources and multiplier at request admission. Later price changes do not retroactively affect an admitted request.

Token rates are integer micro-USD per million tokens. Internally, `u128` accumulates components in 10^-12 USD, applies the multiplier and rounds upward once to micro-USD. Events retain exact pre-rounding decimal integer strings so components, totals, multipliers and rounding can be recomputed. This convention fixes output precision without claiming exact agreement with a provider's monthly-bill rounding, discounts or taxes.

<a id="三种计算器"></a>

## Three calculators

- `chatgpt_v1`: OpenAI input totals include cached tokens. Cache reads/writes are subtracted from ordinary input and priced separately. Reasoning tokens are output details and must not be added again.
- `claude_v1`: Anthropic `input_tokens` excludes cache reads/writes. Ordinary input is charged as reported; cache reads, five-minute writes and one-hour writes are separate. Long-context decisions use total input including cache. Official web search is supported when explicit pricing and usage counts exist.
- `grok_v1`: Supports older Chat formats with separate completion/reasoning counts and newer Responses formats whose output includes reasoning. `total_tokens` validates whether reasoning must be added; ambiguous cases remain pending. Official `cost_in_usd_ticks` is retained unchanged as an actual-cost reference, without replacing the public list-price basis.

Usage is read directly from each provider's native responses. Router does not translate the Agent's generation protocol or change cache, TTL or usage request parameters. Cumulative streaming usage replaces earlier values rather than being summed frame by frame. Only allowlisted numbers and enums are retained, excluding request text, ordinary answers, tool arguments and results.

<a id="报价扩展"></a>

## Quote extensions

Router exports `official_pricing` and `pricing_source` in differential checkpoints. `/admin/pricing` reads or updates the price book; `/admin/public-catalog` returns current pools and quotes for their allowed models. There is no user-facing `/v1/pricing` endpoint.

Model rows in `/admin/pricing` may include `billing_rules`; older price books remain readable. Rules travel with quotes into `merchant_pricing` and the model catalog. Changing them requires a higher official pricing revision.

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

These numbers illustrate the format, not a valid quote for any model. Each variant is a complete tier/speed/geography combination with explicit combined rates; multipliers are not guessed or stacked automatically. `default / standard / global` uses the model's original rates. Pricing follows the actual tier returned upstream; a fallback from an accelerated request to standard service uses standard pricing. `auto` without a reported actual tier cannot be settled.

Anthropic `standard` and `standard_only` normalize to `default`; OpenAI/xAI `fast` normalizes to `priority`. Anthropic `speed: fast` and `inference_geo: us` are independent dimensions. Compatibility settings cannot rewrite these billing dimensions.

By default, long context applies when `input > above_input_tokens`. Where xAI uses an inclusive threshold, `long_context_inclusive: true` changes it to `input >= above_input_tokens`, such as the 200,000-token boundary for Grok 4.7. The applicable tier prices the whole request, not just tokens beyond the threshold.

Anthropic's per-call web-search rate in micro-USD is `tools.web_search`; counts come from native `usage.server_tool_use.web_search_requests`. A missing quote rejects admission; an enabled tool without a returned call count remains pending.

<a id="计费事件"></a>

## Billing events

Root accounting events store the result in `event.billing`; the enclosing event binds the request ID, user, merchant, pool, model and request time. Merchant runtimes do not maintain a second per-record pending queue.

- `state: calculated` includes `version`, `calculator`, `provider`, `basis: official_list_price`, `currency: USD`, pricing revision, multiplier, actual dimensions, raw billing numbers, `items`, `official_cost_picousd`, `charge_numerator`, `charge_denominator`, `rounding`, `official_cost_microusd` and `charge_microusd`.
- `state: pending` includes an explicit `reason` and no debit amount. It cannot be converted into free usage or estimated with an old formula.
- `items[].amount_picousd` is a string. Token items use unit `million_tokens`; tool items use `call`. `charge_numerator / charge_denominator` gives pre-rounding micro-USD, with denominator fixed at `1000000000000`.

Billing results enter signed consumption batches. `pending` or unknown versions must not be interpreted as zero charges or recalculated using an old formula. Historical signed records retain their original bytes. Settlement revisions and boundary validation prevent identical retries from being applied twice.

Independent verifiers can check batch signatures, digests and the signing key's TEE binding; see [Consumption receipts](CONSUMPTION-PROOFS.en.md). See [Local accounting journal](ACCOUNTING-JOURNAL.en.md) for runtime logging and recovery boundaries. Signatures establish provenance and integrity, not complete official usage for every network failure.

<a id="当前边界"></a>

## Current boundaries

- Missing rates for a requested tier or known unsupported paid features return `400 billing_unsupported` before sending a model request.
- Successful responses with incomplete usage, invalid numbers, inconsistent cache splits or arithmetic overflow retain explicit pending reasons. Failed, interrupted and multi-attempt requests follow [Interrupted-request billing](INTERRUPTED-BILLING.en.md): incomplete transmission or explicit rejection is free; interruption while awaiting a reply after full transmission estimates input; interruption after partial output uses official counts plus observed-text estimates. Estimates are explicitly labelled and never presented as official usage.
- Nonstandard tiers, speeds and regions require upstream confirmation. Missing actual dimensions remain pending to avoid charging a higher price after a downgrade.
- Dedicated rules for OpenAI hosted search, file search, code execution, image generation and audio/prediction output are not covered; such requests are rejected. Ordinary client function/custom tools remain supported.
- xAI hosted-tool REST usage may report only aggregate counts without reliable categories. Differently priced tools cannot be treated as equal, so hosted tools are currently rejected; actual-cost fields remain available for reconciliation.
- Claude multi-iteration/automatic-compaction usage crosses separate context boundaries and currently reports `iteration_pricing_unavailable`, rather than combining all iterations to guess a tier.
- Quote calculation and quote collection are separate. Calculators use a validated current price book and do not scrape official pages. See [Price sources](PRICE_SOURCE.en.md) for automatic sources and update failures.

Official references: [OpenAI pricing](https://developers.openai.com/api/docs/pricing), [Claude pricing](https://platform.claude.com/docs/en/about-claude/pricing), [Claude cache usage](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [xAI pricing](https://docs.x.ai/developers/pricing), [xAI Chat usage](https://docs.x.ai/developers/rest-api-reference/inference/chat-completions), [xAI Responses usage](https://docs.x.ai/developers/rest-api-reference/inference/responses), [xAI actual-cost fields](https://docs.x.ai/developers/cost-tracking). Rules last checked: 2026-10-14.
