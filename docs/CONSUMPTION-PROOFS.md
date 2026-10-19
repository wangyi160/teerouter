# Signed consumption batches (v1)

The Router produces independently verifiable **consumption** evidence. Payments,
deposits, refunds, withdrawals, administrative corrections and externally supplied
balances are not attested financial transactions. They remain management/payment
provider responsibilities. No blockchain or synchronous per-request management call.

## Export and durability

`POST /admin/checkpoint/diff` accepts `sign_receipts:true` in addition to `boot_id`
and `base_id`. Checkpoint envelopes advertise `receipt_proof_version:1`.
Once initialized, a signed export freezes an exact recovery snapshot and consumption
batch until acknowledgement. Repeating an export returns the same signed bytes;
concurrent requests continue recording into the next batch. Diff semantics are unchanged.

`receipt_batch` contains `manifest` (an exact UTF-8 JSON string), `signature`
(lowercase Ed25519 hex), `batch_hash` and private `records` (`payload`, `salt`).
Management verifies signatures, record order/root/summary, exact correspondence to
the exported events, and its previously persisted head. It commits the raw evidence,
settlement, cursor and recovery snapshot in one durable transaction, then sends
`receipt_batch_hash` with `POST /admin/checkpoint/ack`. Wrong/missing proof hashes
cannot clear a pending signed batch. Lost ACK retries never charge twice.

Once a signed head exists, subsequent checkpoint exports remain signed. A manager
that has adopted this protocol refuses unsigned checkpoints, including downgrades.
Proof records are stored separately from mutable settlement status and cannot be
silently replaced by a manual accounting adjustment.

Management persists the new `receipt_head` and empty `events` in recovery state,
even if the ACK response is lost. Initialization refuses replayed historical events.
The signing key is generated inside each Router process, never exported/persisted.
On restart the new key/boot ID points to the supplied last durable head and labels
the batch `restart`. This is **not rollback prevention**: independent witnesses
are required to detect a malicious manager withholding/forking the durable head.

Normal automatic synchronization is 300 seconds. Manual operations also generate
real batches. A restart may lose everything since the last successful persistence;
long disconnections increase this window. Missing records are never invented or
billed. There is no promise of complete history or global publication availability.

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
