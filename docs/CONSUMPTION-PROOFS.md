# Signed consumption batches

The Router produces independently verifiable **consumption** evidence. Payments,
deposits, refunds, withdrawals, administrative corrections and externally supplied
balances are not attested financial transactions. They remain management/payment
provider responsibilities. No blockchain or synchronous per-request management call.

## Autonomous public relay

The optional `teerouter.public-consumption-batch.v1` protocol publishes consumption
directly to an HTTPS receipt mailbox. It does not require management to be online.
The existing authenticated, attested management channel configures
`PUT /admin/receipts/relay` with `url` (HTTPS origin), `upload_token`,
`interval_seconds` (default 300), and `user_ids` (user ID to private random
32-byte hex identity). Secrets and private identities are never included in
public batches or status. Destination and existing private identities cannot be
changed after activation. Tokens can be rotated through the same endpoint.

Management verifies the receipt signing key's TEE evidence and registers it with
the mailbox before enabling publication. The Router holds only an upload token;
the manager's separate registration/archive credential is not sent to the Router.
`GET /admin/receipts/relay` exposes redacted status.
`POST /admin/receipts/relay/flush {}` forces a pending upload.

Every nonempty batch uses a fresh public 32-byte nonce. Public identifiers are:

```
user_hash = SHA256(private_user_id_bytes || batch_nonce_bytes)
key_hash  = SHA256(UTF8("teerouter:receipt-key-id:v1\0")
                   || private_user_id_bytes || UTF8(key_id) || 0x00
                   || batch_nonce_bytes)
```

Public records contain sequence, rotating pseudonyms, merchant/pool/model,
status, timing, usage, pricing and calculated charge. Refusals contain only a
structured type. Request/response bodies, refusal text, account credentials,
usernames and persistent user/API-key IDs are excluded. Traffic patterns can
still permit correlation; this is pseudonymization, not a claim of anonymity.

Each envelope contains `manifest` and `records_json` as exact UTF-8 JSON strings,
plus hex `signature` and `batch_hash`. The manifest includes schema, algorithm,
instance/boot/public key, batch number, previous batch hash, creation time,
sequence range, batch nonce, record count, records hash and USD micro-unit total.

```
message        = UTF8("teerouter:public-consumption-batch:v1\0") || UTF8(manifest)
signature      = Ed25519.sign(process_private_key, message)
batch_hash     = SHA256(message)
records_sha256 = SHA256(UTF8(records_json))
```

Upload is `PUT /v1/streams/{instance_id}/batches/{batch_no}` with bearer auth.
A storage ACK must contain `durable:true`, the matching `batch_hash`,
`detail_state:"pending"` or `"archived"`, and `settlement_acknowledged:false`.
Retries reuse identical signed bytes; batches are limited to 512 records and a
bounded byte size. Failed uploads retry after 30 seconds.

Storage ACK frees raw event details but **does not change balances or clear
consumption**. Per-user, per-batch consumption summaries stay in memory until
management durably verifies and records them. Bounded queues apply backpressure
instead of silently discarding accepted events.

Management persists verified records, accounting, cursor, recovery state and an
archive-confirmation outbox atomically before asking the mailbox to remove
details. The signed envelope remains public. Financial acknowledgement is
separate: `POST /admin/receipts/relay/settle` supplies `boot_id`, a monotonically
increasing `revision`, the verified `through` head, and exactly one of `balances`
or `balances_patch`. The Router atomically updates balances and retires only
consumption through that head, preserving newer requests. Exact retries are
idempotent; stale revisions and unknown batch heads are rejected.

Before restoring a crashed Router, management ingests all mailbox batches and
initializes from the last durable head and resulting balances. An unuploaded
crash tail can be lost. The new process signs subsequent batches with a new key
and boot ID. This does not prove complete history, prevent malicious rollback,
or attest payments, deposits or withdrawals.

The independent verifier supports both public batches and the legacy private
protocol below. A detached public envelope proves its signature and committed
records hash; verifying records also requires the original `records_json`.

## Historical private proofs

Historical private proof bundles remain verifiable using their original manifest and Merkle format. Their checkpoint ACK API has been removed. New consumption records use the autonomous public relay above: signed upload, management archive, then relay settlement. Checkpoint diff is used for configuration/recovery state and is not the consumption-record transport.

The per-boot signing key is generated inside Router. A restart links the new boot to the last durable head; this does not itself prevent a malicious manager from withholding or forking history. Independent retained public heads are needed to detect rollback.

## Cryptography

All fixed-size bytes below use lowercase hex in JSON. No floating-point amounts.

```
message    = UTF8("teerouter:consumption-batch:v1\0") || UTF8(manifest)
signature  = Ed25519.sign(process_private_key, message)
batch_hash = SHA256(message)
leaf       = SHA256(0x00 || random_32_byte_salt || UTF8(record.payload))
parent     = SHA256(0x01 || left_32_bytes || right_32_bytes)
empty_root = SHA256(empty_bytes)
```

Records are ordered by increasing `seq`. Odd final nodes are duplicated at each
tree level. Proofs include leaf index and sibling hashes bottom-up; the signed
count determines depth/direction and prevents arbitrary paths. A validator checks
the record's sequence equals `sequence_before + index + 1`. Public manifests
commit to ranges, counts, priced-count, total calculated charge, boot/instance,
previous batch hash and previous boot. Unpriced/failed usage is not silently zero-rated.

`POST /admin/receipts/evidence {"nonce":"<fresh 32-byte hex>"}` returns the
process public key, boot ID and hardware evidence over:

```
REPORT_DATA = SHA512(UTF8("teerouter:receipt-key:v1\0") || nonce_32_bytes
                     || public_key_32_bytes || boot_id_16_bytes)
```

The endpoint signs no manager-supplied financial digest. Evidence generation uses
the measured local guest interface with no remote attester or production simulator
fallback. Separate attested TLS binding remains unchanged. Evidence may be cached
per process key; normal batches require only local Ed25519 signing. The standalone
verifier supports the deployed dstack TDX evidence and rejects unsupported evidence.

Public bundles contain `batch` without `records`, plus `identity`. Optional
`inclusions` contain the selected private `payload`, `salt`, `index`, and `siblings`.
Public downloads must never include whole checkpoints, account secrets, request
bodies, personal bill details, or authenticated management configuration.

## Portal and verifier

`/portal/receipts`: public batch explorer. `/portal/receipts/mine`: authenticated
owner-scoped receipts. API under `/portal/api/receipts/` provides cursor pagination,
public batch downloads and private individual inclusion proofs. A stale publication
indicator and explicit restart marker describe known gaps without fabricating ticks.

Build/run the independent [verifier](../verifier/README.md) with a separately reviewed
TDX release policy. Client applications do not need embedded receipt validation.
