# Router HTTP API

Router exposes 39 explicit method/path operations (implicit HEAD for GET is not counted). All operations require credentials. This index describes only handlers implemented by Router. Control endpoints are part of its own HTTP contract.

## Model forwarding

| Method | Path | Authentication |
| --- | --- | --- |
| POST | `/v1/responses` | User Bearer + client Ed25519 proof |
| POST | `/v1/messages` | User Bearer + client Ed25519 proof |
| POST | `/v1/chat/completions` | User Bearer + client Ed25519 proof |

See [user request signatures](USER-AUTH.md). Router checks agent/provider compatibility, access, routing policy, limits and balance before forwarding. Native request parameters remain unchanged except the qualified routing model ID is resolved to its upstream model ID.

OpenAI API-key accounts use `https://api.openai.com/v1`; ChatGPT OAuth accounts
use `https://chatgpt.com/backend-api/codex` and the native Responses protocol.
The latter is the account-backed Codex channel, distinct from ChatGPT web chat.
Router preserves the selected channel's request parameters and credentials.

The model request body ceiling, `server.max_body_bytes`, defaults to 536,870,912
bytes (512 MiB). This covers the 512 MB total payload documented by the
[OpenAI vision API](https://developers.openai.com/api/docs/guides/images-vision)
as checked on 2026-10-29. It counts the entire serialized JSON, including Base64
images and conversation history, and applies with or without `Content-Length`.
Explicit configuration overrides the default. This is a Router resource ceiling;
it does not establish the ChatGPT account backend's limit or override model,
image, context, or other provider limits. The upstream validates those limits.
Router forwards image inputs without resizing or recompressing them.

Body failures have distinct responses:

| HTTP status | Error code | Meaning |
| --- | --- | --- |
| 413 | `body_too_large` | The received body exceeds Router's configured ceiling; it was not forwarded. |
| 400 | `body_read_error` | Reading the incoming body failed; it was not forwarded. |
| 408 | `body_timeout` | Receiving the body exceeded `server.body_timeout_secs`; it was not forwarded. |
| 413 | `upstream_body_too_large` | The provider returned HTTP 413; Router preserves that status without retrying the request. |

Concurrent requests still share the ingress and generation concurrency limits.
The ceiling does not reserve that much memory per request; received bodies and
their parsed JSON do consume memory while being processed.

## Attestation and health

| Method | Path | Authentication |
| --- | --- | --- |
| POST | `/.well-known/teellm/attestation` | Handshake JWT in `x-router-access` |
| GET | `/health/live` | Handshake JWT or admin Bearer |
| GET | `/health/ready` | Handshake JWT or admin Bearer |

`/health/live` advertises `user_auth_version: 2`, the current `boot_id` and routing capabilities. The handshake JWT cannot authorize model or administrative operations.

## Merchant direct channel

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/merchant/bind` | Bind the locally owned merchant control credential |
| POST | `/merchant/unlock` | Open the merchant runtime after Router startup |
| PUT | `/merchant/accounts/sync` | Upload accounts, access tokens and private proxy material |
| PUT | `/merchant/accounts/access` | Replace access-token material |
| POST | `/merchant/accounts/test` | Test draft credentials through their proxy |

These operations require merchant enrollment plus the merchant-owned control credential over an attested connection. Refresh tokens are rejected. Public configuration uses admin-authenticated control endpoints; there is no credential staging ticket or merchant ledger query/ACK channel.

## Router control interfaces

All operations below require the Router admin Bearer. This authority cannot export merchant credentials or sign user requests.

| Method | Path | Purpose |
| --- | --- | --- |
| GET / PUT | `/admin/merchants` | Merchant registrations and admission |
| GET | `/admin/merchant-views` | Public merchant snapshots |
| GET | `/admin/merchants/{mid}/view` | One public merchant snapshot |
| PUT | `/admin/merchants/{mid}/config` | Public configuration command |
| GET | `/admin/merchants/{mid}/commands/{oid}` | Confirm an idempotent command |
| GET | `/admin/public-catalog` | Read-only pool inventory snapshot |
| GET / PATCH | `/admin/config` | Read/update root configuration |
| POST | `/admin/apply` | Atomic users, permissions and root configuration update |
| POST | `/admin/checkpoint/diff` | Incremental checkpoint; missing/stale base returns full state |
| POST | `/admin/initialize` | Initialize/restore a new boot |
| POST | `/admin/receipts/evidence` | Attested receipt signing identity |
| GET / PUT | `/admin/receipts/relay` | Receipt upload configuration/status |
| POST | `/admin/receipts/relay/flush` | Flush pending signed batches |
| POST | `/admin/receipts/relay/settle` | Acknowledge archived batches and apply balances |
| GET / PUT | `/admin/pricing` | Official price book / versioned price updates |
| GET / PUT | `/admin/pricing/source` | Price source configuration |
| POST | `/admin/pricing/refresh` | Refresh the configured price source |
| GET | `/admin/status` | Runtime status |
| GET | `/admin/usage` | Aggregate accounting status |
| GET | `/admin/logs` | Operational audit records |
| GET | `/admin/metrics` | Runtime/storage/discovery metrics |
| POST | `/admin/quiesce` | Drain/resume requests |
| POST | `/admin/maintenance` | Explicit storage maintenance |

Legacy user queries, user CRUD, root upstream-account CRUD/credential export, standalone refusal queues, full-checkpoint GET/ACK and model-discovery GET/refresh routes have been removed, including their handlers. They are not compatibility aliases. Unknown paths return 404 after authentication; unsupported methods on retained routes return 405.

## 请求路由规则 v1

`X-Router-Routing` contains one JSON object, at most 2048 bytes:

```json
{"version":1,"mode":"auto","max_multiplier_millionths":1000000,"priority":"price"}
```

`mode` is `auto` or `fixed`; `fixed` requires `pool` in the form
`m_sig_<public-key-sha256>.pool`, while `auto` forbids it. `priority` is `price`
(default) or `speed`. The maximum multiplier is an integer in millionths,
bounded by 1,000,000,000,000,000. Unknown fields and duplicate headers are rejected.

The rule cannot be combined with `x-router-pool`, `x-router-account`,
`x-router-price-version` or `x-router-official-price-revision`; this returns
`400 routing_rule_conflict`. Router applies provider/agent constraints, customer
authorization, pool policy, capability evidence, quote validity and concurrency
before selecting a candidate. Price ranking includes applicable cache costs;
speed ranking uses account-proxy HTTPS latency, not model-generation throughput.
Existing stateful context stays with its original account. An unknown response
or provider file may require an explicit original pool.

Without this rule, a merchant-qualified model or `x-router-pool` must determine
the merchant; otherwise Router returns `400 merchant_required`. Details and
privacy boundaries are in the [merchant protocol](MERCHANTS.md).
