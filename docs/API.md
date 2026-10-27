# Router HTTP API

Router exposes 39 explicit method/path operations (implicit HEAD for GET is not counted). All operations require credentials. User and merchant catalog, pricing and ledger reads belong to the management platform; teeproxy serves its local `/v1/models` from that platform.

## Model forwarding

| Method | Path | Authentication |
| --- | --- | --- |
| POST | `/v1/responses` | Platform Bearer + client Ed25519 proof |
| POST | `/v1/messages` | Platform Bearer + client Ed25519 proof |
| POST | `/v1/chat/completions` | Platform Bearer + client Ed25519 proof |

See [user request signatures](USER-AUTH.md). Router checks agent/provider compatibility, access, routing policy, limits and balance before forwarding. Native request parameters remain unchanged except the qualified routing model ID is resolved to its upstream model ID.

## Attestation and health

| Method | Path | Authentication |
| --- | --- | --- |
| POST | `/.well-known/teellm/attestation` | Login-issued handshake JWT in `x-router-access` |
| GET | `/health/live` | Handshake JWT or admin Bearer |
| GET | `/health/ready` | Handshake JWT or admin Bearer |

`/health/live` advertises `user_auth_version: 1`, the current `boot_id` and routing capabilities. The handshake JWT cannot authorize model or administrative operations.

## Merchant direct channel

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/merchant/bind` | Bind the locally owned merchant control credential |
| POST | `/merchant/unlock` | Open the merchant runtime after Router startup |
| PUT | `/merchant/accounts/sync` | Upload accounts, access tokens and private proxy material |
| PUT | `/merchant/accounts/access` | Replace access-token material |
| POST | `/merchant/accounts/test` | Test draft credentials through their proxy |

These operations require merchant enrollment plus the merchant-owned control credential over an attested connection. Refresh tokens are rejected. Public configuration travels through management; there is no credential staging ticket or merchant ledger query/ACK channel.

## Platform administration

All operations below require the platform admin Bearer. This authority cannot export merchant credentials or sign user requests.

| Method | Path | Purpose |
| --- | --- | --- |
| GET / PUT | `/admin/merchants` | Registrations and platform admission |
| GET | `/admin/merchant-views` | Public merchant snapshots |
| GET | `/admin/merchants/{mid}/view` | One public merchant snapshot |
| PUT | `/admin/merchants/{mid}/config` | Public configuration command |
| GET | `/admin/merchants/{mid}/commands/{oid}` | Confirm an idempotent command |
| GET | `/admin/public-catalog` | Management inventory snapshot |
| GET / PATCH | `/admin/config` | Read/update platform configuration |
| POST | `/admin/apply` | Atomic platform users, permissions and configuration update |
| POST | `/admin/checkpoint/diff` | Incremental checkpoint; missing/stale base returns full state |
| POST | `/admin/initialize` | Initialize/restore a new boot |
| POST | `/admin/receipts/evidence` | Attested receipt signing identity |
| GET / PUT | `/admin/receipts/relay` | Receipt upload configuration/status |
| POST | `/admin/receipts/relay/flush` | Flush pending signed batches |
| POST | `/admin/receipts/relay/settle` | Acknowledge archived batches and apply balances |
| GET / PUT | `/admin/pricing` | Price book / permitted test-provider prices |
| GET / PUT | `/admin/pricing/source` | Price source configuration |
| POST | `/admin/pricing/refresh` | Refresh the configured price source |
| GET | `/admin/status` | Runtime status |
| GET | `/admin/usage` | Aggregate accounting status |
| GET | `/admin/logs` | Operational audit records |
| GET | `/admin/metrics` | Runtime/storage/discovery metrics |
| POST | `/admin/quiesce` | Drain/resume requests |
| POST | `/admin/maintenance` | Explicit storage maintenance |

Legacy user queries, user CRUD, platform upstream-account CRUD/credential export, standalone refusal queues, full-checkpoint GET/ACK and model-discovery GET/refresh routes have been removed, including their handlers. They are not compatibility aliases. Unknown paths return 404 after authentication; unsupported methods on retained routes return 405.
