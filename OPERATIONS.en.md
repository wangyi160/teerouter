<a id="router-状态与同步协议"></a>

# Router state and synchronization protocol

[简体中文](OPERATIONS.md) | **English**

This document describes state transitions following Router control requests. All `/admin/*` endpoints require an administrator Bearer. User and merchant credentials cannot read root checkpoints or initialize the root runtime.

<a id="初始化"></a>

## Initialization

Every process start generates a new `boot_id`. Business traffic remains unavailable until initialization succeeds. Without a valid baseline, `POST /admin/checkpoint/diff` returns the complete current `state` and boot identifier.

`POST /admin/initialize` accepts:

```json
{"boot_id":"CURRENT_BOOT_ID","state":null}
```

`state:null` initializes an empty state. Recovery supplies a complete `state` matching the current schema. Router validates the boot identifier, configuration, identities, sequences, balances and relay boundaries. Initialization may succeed only once per boot. Root checkpoints exclude merchant account secrets; merchant runtimes establish private state through direct account synchronization.

Recovery input must not carry unsettled relay summaries: `retained` must be empty and `uploaded` must equal `settled`. Boundaries must also agree with usage sequences. With the authenticated journal enabled, Router additionally validates the journal against recovery boundaries and recovers its committed tail. A failed recovery is not replaced with a fresh empty ledger.

<a id="差量检查点"></a>

## Differential checkpoints

`POST /admin/checkpoint/diff` accepts only `boot_id` and `base_id`, both of which may initially be null. The response contains `checkpoint_diff_version:1`, a new `checkpoint_id`, and either the full `state` or `changes` relative to `base_id`:

```json
{"path":["object-key","child-key"],"value":"new-value"}
```

Deletion is represented by `{"path":["object-key"],"remove":true}`. Objects are compared field by field; arrays are replaced as a whole; null is distinct from deletion. A restart, missing baseline or baseline replaced by another reader causes a full-state response. Router retains only one differential baseline.

Reading a checkpoint does not create private signed batches, acknowledge uploads or settle consumption. `sign_receipts` is an unknown field and returns 400. The old full-checkpoint GET/ACK endpoints do not exist.

`storage.manager_connected` is a retained status-field name: it is true when a valid differential checkpoint was read within the last 630 seconds. It does not control business admission and is not a heartbeat lease.

<a id="结算与余额"></a>

## Settlement and balances

Available balance equals the most recently synchronized balance minus unsettled consumption. Model requests only accumulate consumption. Upload acknowledgments only release the corresponding details; they cannot update balances or clear consumption.

`POST /admin/receipts/relay/settle` accepts the current `boot_id`, an increasing `revision`, an uploaded batch boundary `through`, and exactly one of `balances` or `balances_patch`. Router atomically updates synchronized balances and clears consumption within the boundary while preserving later consumption. Identical retries are idempotent. Old revisions, conflicting contents, unknown boundaries and incorrect boot IDs are rejected. See [Consumption receipts](docs/CONSUMPTION-PROOFS.en.md) for signatures and batch formats.

Users and permissions are updated atomically through `/admin/apply`. Null in a merge patch deletes a field; full configurations still undergo strict field validation. Configuration updates and settlements use separate revisions and state boundaries.

<a id="排空状态"></a>

## Draining

`POST /admin/quiesce` accepts `{"boot_id":"CURRENT_BOOT_ID","quiescing":true}`. While draining, new model requests, merchant operations and configuration writes return `503 router_draining`. Existing requests and background tasks already underway may finish. Control reads, checkpoints, relay flushes and settlements remain available.

`lifecycle.drained:true` in `/admin/metrics` means neither root nor merchant runtimes have in-flight requests or background tasks. Sending `quiescing:false` resumes admission. The flag applies only to the current process and is not written into recovery checkpoints.

<a id="凭据与拒绝边界"></a>

## Credentials and rejection boundaries

Router uses `access-token-only-v1`: it neither accepts refresh tokens nor performs OAuth renewal. Expiry comes from `expires_at` in Unix seconds, Claude `expiresAt` in milliseconds, or JWT `exp`. Without a parseable expiry, expiration cannot be anticipated.

Expired accounts report `health.state:"expired"`, `health.last_error:"access_token_expired"` and `refresh_mode:"external"`, and stop accepting new requests. Credentials rejected by the official provider before expiry report `reauthorization_required`. New access credentials take effect only after validation through merchant account endpoints.

Accepted providers are `openai`, `anthropic` (alias `claude`) and `xai` (alias `grok`). Full configurations reject `gemini_project`, `client_secret_ref`, `config.moderation` and merchant `backup`, including null values. An empty historical `state.moderation:{}` can be read but is no longer exported; nonempty historical values are rejected rather than silently discarded. See [Consumption receipts](docs/CONSUMPTION-PROOFS.en.md) for old journal records containing an empty `receipt_head:null`.

<a id="上游拒答审计"></a>

## Upstream refusal auditing

Refusal detection is enabled by default, with `moderation_mode:"upstream_refusal"` and `refusal_audit_enabled:true`. Ordinary natural-language refusals are not inferred from keywords. Explicit refusals retain the original response status and do not trigger an account-switching retry.

On completion, failure, timeout or cancellation, refusal category is recorded together with usage. Public batches contain `refusal_type`, not refusal text. Merchant counters persist separately from individual records. Historical refusal queues remain subject to recovery validation, but have no dedicated HTTP query/delete endpoints.

<a id="耐久性边界"></a>

## Durability boundaries

Without a local journal, unsynchronized in-memory state can be lost after an abnormal exit. The authenticated journal can recover only successfully committed records, not unfinished usage that was never generated or committed. A write failure closes new admission, but an uncommitted tail may still be lost. See [Accounting journal](docs/ACCOUNTING-JOURNAL.en.md) and [Interrupted-request billing](docs/INTERRUPTED-BILLING.en.md).
