<a id="router-商户协议"></a>

# Router merchant protocol

[简体中文](MERCHANTS.md) | **English**

All upstream accounts and pools belong to isolated merchant runtimes. The root runtime holds user permissions, public merchant registrations and unified consumption records; its `models` and `accounts` must be empty. Control interfaces cannot export merchant credentials.

<a id="身份与认证"></a>

## Identity and authentication

A merchant ID is `m_sig_` followed by the lowercase SHA-256 hex digest of its 32-byte public key. The admission credential is a 32-byte random value encoded as 64 lowercase hex characters; `credential_hash` is the SHA-256 digest of that credential text. The credential does not determine the merchant ID.

`PUT /admin/merchants` requires an administrator Bearer and accepts this registration format:

```json
{"id":"m_sig_<PUBLIC_KEY_SHA256>","public_key":"<32_BYTE_PUBLIC_KEY_HEX>","credential_hash":"<64_CHARACTER_DIGEST>","name":"Merchant name","enabled":true,"max_concurrent":16,"rpm":0,"tpm":0,"users":["u_sig_<USER_PUBLIC_KEY_SHA256>"]}
```

The merchant channel additionally requires an independent 32-byte control key. Its Bearer is `<admission-credential>.<control-key>`. `POST /merchant/bind` initially binds the control-key digest as `owner_hash`; subsequent requests validate both components. Registration updates cannot replace the bound digest. Administrator and user Bearers cannot access the private merchant channel.

Private merchant requests must use the same connection on which TEE evidence and the current TLS binding were verified. Before initial binding, another holder of the admission credential could bind first. Only a binding response matching the expected control key confirms that account secrets will be handed to the intended identity. Router has no control-key retrieval API.

<a id="公开控制与私有账号"></a>

## Public control and private accounts

| Channel | Endpoint | Router behavior |
| --- | --- | --- |
| Administrator | `GET /admin/merchants` | Returns registrations and unlock status |
| Administrator | `GET /admin/merchant-views` | Returns sanitized state for unlocked merchants and IDs of locked merchants |
| Administrator | `GET /admin/merchants/{mid}/view` | Returns one merchant's sanitized state |
| Administrator | `PUT /admin/merchants/{mid}/config` | Updates public pool policies, multipliers and customer limits |
| Administrator | `GET /admin/merchants/{mid}/commands/{oid}` | Retrieves an idempotent configuration-command receipt |
| Merchant owner | `POST /merchant/bind` | Binds the control-key digest |
| Merchant owner | `POST /merchant/unlock` | Creates an in-memory runtime; request includes `minimum_revision` |
| Merchant owner | `PUT /merchant/accounts/sync` | Validates and activates private accounts, pool associations and secrets |
| Merchant owner | `PUT /merchant/accounts/access` | Conditionally replaces existing access credentials |
| Merchant owner | `POST /merchant/accounts/test` | Tests temporary credentials and a proxy without saving configuration |

Public configuration commands contain `operation_id`, `runtime_id` and `config`. `config.expected_revision` enables revision comparison. Identical operation IDs and contents are idempotent; conflicting contents fail. This channel rejects `accounts`, `secrets` and plaintext proxies; proxy fields accept only `[redacted]` placeholders for existing secrets. Nonempty `compatibility` is rejected.

Public `config` supports `pools`, `pool_pricing`, `max_concurrent`, `rpm`, `tpm`, `restrictions`, `customer_defaults` and `customers`. Pool policies can restrict exposed models through `allowed_models`. `account_controls` entries containing only `enabled` can enable or disable accounts. Validation is implemented in `src/merchants/control.rs`.

Private account synchronization contains `operation_id`, `runtime_id`, `expected_revision` and `pools`, plus optional `accounts`, `secrets`, `delete_accounts`, `delete_secrets` and `new_pool_pricing`. Initial pricing can accompany new pools. Account synchronization does not overwrite existing pool multipliers, model allowlists or customer limits. The old `accounts/prepare` and ticket-submission channels have been removed.

Account proxies enter through private synchronization: `deployments[].proxy` for API-key accounts, or `proxy` within OAuth `accounts`. Only supported HTTP(S), SOCKS5h or [VLESS](VLESS.en.md) proxies are accepted; upstream addresses remain fixed official domains. Account configuration rejects local OAuth files, custom upstream URLs and inter-pool fallback.

<a id="账号测试"></a>

## Account tests

`POST /merchant/accounts/test` accepts `pool`, `proxy`, access-only `credential` or `api_key`, and optional `model`. Unpublished pools use [draft_pool](MERCHANT-POOLS.en.md). Router first reads the model catalog, then selects an allowed text-priced model and sends a short request with at most 32 output tokens.

Success returns `ok:true`, `model`, `model_count`, `provider` and `latency_ms`. Failure returns `ok:false`, `stage`, a sanitized error `code` and an upstream status when available. Merely listing models does not count as a successful test. The overall timeout is 75 seconds, with at most one concurrent test per merchant and eight globally. Tests neither save credentials, renew tokens nor create user consumption events; the real upstream may charge for the request.

<a id="池模型与费用"></a>

## Pools, models and charges

Each pool has one `multiplier_millionths`: `1000000` means 1×, `200000` means 0.2×, with a maximum multiplier of 1,000,000,000×. Input, output, cache and long-context charges all use this multiplier, with final rounding defined by the protocol. Nonempty legacy `prices` updates are rejected.

At request admission, `merchant_pricing` freezes base rates, multiplier, version, sources and rules. Missing multipliers or base prices, or expired prices, reject admission. Later price changes do not affect existing requests. See [Billing](BILLING.en.md) for fields and boundaries.

`allowed_models` belongs to a pool, not an individual account. Omitted or null allows all models verified against the official catalog; an empty array allows none. It accepts at most 4096 distinct official raw IDs, without wildcards. Writing a nonexistent model does not create capability. Changes affect subsequent admission only.

Official catalogs are shared across OpenAI, Claude and Grok respectively; each account's credentials, proxy, health and limits remain independent. `discovery.refresh_secs` defaults to 300, with background checks at most every 30 seconds. The default total refresh budget is 15 seconds, and concurrent refreshes coalesce. A failed source can be replaced; if every source fails or none succeeds within budget, that provider's catalog is cleared. The next cycle retries, without admitting requests using an invalid catalog. Synchronizing an account does not establish generation capability.

Catalog status includes `scope:"provider"`, `provider` and `refresh_secs`, without revealing the source merchant or `source_deployment`. Merchant `catalog.data` is organized around that merchant's accounts; accounts from the same provider reference the same catalog evidence.

<a id="用户请求与选路"></a>

## User requests and routing

[Request routing rules v1](API.en.md#请求路由规则-v1) specify a fixed pool or automatic matching, multiplier ceiling and price/latency priority. Model namespaces are `chatgpt/`, `claude/` and `grok/`. Router constrains the provider before checking pools, permissions, capabilities and pricing. Upstream requests use official model IDs.

Without routing rules, `x-router-pool: m_sig_<PUBLIC_KEY_SHA256>.pool-name` or merchant-qualified model names remain supported. Insufficient information to determine a merchant returns `400 merchant_required`; there is no implicit direct account. Old account/catalog control paths have no compatibility aliases.

<a id="客户可见性与拒答阈值"></a>

## Customer visibility and refusal thresholds

A customer becomes visible only after an upstream response is received, including generation refusals and HTTP errors. Preauthorization, catalog reads, local rejection and connection failures without a response do not establish visibility. Merchant state excludes the preauthorized `merchant.users` list.

`customer_defaults` sets default `enabled`, `models`, concurrency, RPM/TPM and `refusal_limit`. `customers` updates visible customers; omitting a customer preserves existing permissions, while disabling requires explicit `enabled:false`. Limit configuration cannot itself establish visibility.

The default refusal threshold is 0, meaning unlimited. A null or omitted customer value inherits the default. The range is 0–1,000,000,000. Once the count strictly exceeds the threshold, new requests return `403 merchant_refusal_limit_exceeded`. A fixed pool cannot bypass the limit; automatic routing may choose another permitted merchant.

Counters are keyed by merchant ID and user ID across all keys, pools and models. Each request counts at most one explicit refusal; ordinary network errors, 429s and ordinary 5xx responses do not count. `customer_refusals` exposes `total`, `count`, `limit` and `blocked` for visible customers only.

To reset, submit `refusal_baseline:total` and enable the customer. Baselines increase monotonically, preserving refusals added after the baseline was read. Cumulative totals are independent of the per-record queue and, with journaling enabled, persist in the same transaction. Without a journal, an unsynchronized tail can still be lost.

<a id="access-token-only-v1-与恢复"></a>

## Access-token-only v1 and recovery

`merchant.credential_protocol` is `access-token-only-v1`. Only access tokens, expiry and explicitly supported metadata are accepted. Refresh tokens, empty refresh fields, nested refresh fields and unknown fields are rejected. These rules apply to initialization, account synchronization, disabled accounts and temporary tests.

Incremental `PUT /merchant/accounts/access` request:

```json
{"runtime_id":"CURRENT_MERCHANT_RUNTIME","accounts":{"alias":{"expected_access_sha256":"OLD_ACCESS_DIGEST","credential":{"access_token":"NEW_ACCESS_TOKEN","expires_at":2000000000}}}}
```

An old runtime returns `merchant_runtime_changed`; a stale digest returns `account_credential_changed`; a prohibited refresh field returns `access_token_only`. Identical retries do not increase account versions. This endpoint does not change pool, customer or merchant configuration revisions. Response field `accounts.data[].access_sha256` is visible only to the owner.

After restart, private merchant runtimes are empty; `configuration_required:true` indicates that private configuration is still needed. Public checkpoints exclude account secrets, and registration/recovery reject legacy `backup` fields. Existing access tokens stop being used after expiry or official rejection until valid replacement credentials arrive.

Consumption is recorded once in the root runtime; merchants have no second pending queue. See [Merchant accounting boundaries](MERCHANT-LEDGER.en.md), [USER-AUTH](USER-AUTH.en.md) for user signatures, and [API](API.en.md) for the complete endpoint list.
