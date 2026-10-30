<a id="签名消费批次"></a>

# Signed consumption batches

[简体中文](CONSUMPTION-PROOFS.md) | **English**

Router produces verifiable evidence for its consumption records. A signature
authenticates the committed bytes; it does not prove complete history, the latest
state, or the correctness of an externally supplied balance.

<a id="公开上传协议"></a>

## Public relay protocol

`teerouter.public-consumption-batch.v1` publishes signed batches to an HTTPS
receipt endpoint. `PUT /admin/receipts/relay` requires admin authentication and
accepts `url` (HTTPS origin), `upload_token`, `interval_seconds` (default 300),
and `user_ids` (user ID to private random 32-byte hex identity).

Secrets and private identities are excluded from public batches and status.
After activation, the destination and existing private identities cannot change;
upload tokens can rotate. `GET /admin/receipts/relay` returns redacted state.
`POST /admin/receipts/relay/flush {}` requests an immediate upload.

Every nonempty batch uses a fresh public 32-byte nonce:

```text
user_hash = SHA256(private_user_id_bytes || batch_nonce_bytes)
key_hash  = SHA256(UTF8("teerouter:receipt-key-id:v1\0")
                   || private_user_id_bytes || UTF8(key_id) || 0x00
                   || batch_nonce_bytes)
```

Public records contain sequence, rotating pseudonyms, merchant/pool/model,
status, timing, usage, pricing and calculated charge. Refusals contain only a
structured type. Bodies, refusal text, credentials, usernames and persistent
user/key IDs are excluded. Timing and traffic patterns may still correlate
records; rotating identifiers do not guarantee anonymity.

Each envelope contains `manifest` and `records_json` as exact UTF-8 JSON strings,
plus hex `signature` and `batch_hash`. The manifest commits to schema, algorithm,
instance/boot/public key, batch number, previous hash, creation time, sequence
range, batch nonce, record count, records hash and the micro-USD total.

```text
message        = UTF8("teerouter:public-consumption-batch:v1\0") || UTF8(manifest)
signature      = Ed25519.sign(process_private_key, message)
batch_hash     = SHA256(message)
records_sha256 = SHA256(UTF8(records_json))
```

There is one signature per batch, not one per record. Public records do not use
Merkle inclusion proofs. Verifying their bytes requires the original complete
`records_json`; a detached envelope proves only the signature and committed hash.

<a id="上传与结算边界"></a>

## Upload and settlement boundaries

Router sends `PUT /v1/streams/{instance_id}/batches/{batch_no}` to the configured
receipt endpoint with its upload Bearer. An accepted storage ACK must contain
`durable:true`, the matching `batch_hash`, `detail_state:"pending"` or
`"archived"`, and `settlement_acknowledged:false`.

Batches are limited to 512 records and a bounded byte size. Retries preserve the
exact frozen signed bytes. An upload failure ends that cycle; retry occurs in
the next `interval_seconds` cycle. There is no separate 30-second retry loop.

Storage ACK releases the corresponding raw details but does not change balances
or clear consumption. Per-user/per-batch summaries remain until settlement.
Bounded queues reject new admissions rather than silently evict accepted events.

`POST /admin/receipts/relay/settle` takes the current `boot_id`, increasing
`revision`, uploaded `through` head, and exactly one of `balances` or
`balances_patch`. Router atomically applies balances and retires consumption
only through that head, preserving newer requests. Identical retries are
idempotent; stale revisions, conflicting retries and unknown heads are rejected.

Recovery validates the checkpoint head and balances, then reconciles committed
local journal records when that journal is enabled. New batches use the new
process key and boot ID; a retained frozen batch keeps its original signature.
Without a successful durable write, an unuploaded tail can be lost. Neither
signatures nor the journal alone prove that history was not withheld or rolled
back. See [state synchronization](../OPERATIONS.en.md) and the
[accounting journal](ACCOUNTING-JOURNAL.en.md).

<a id="检查点与历史证据"></a>

## Checkpoints and historical evidence

`POST /admin/checkpoint/diff` accepts only `boot_id` and `base_id`. The retired
`sign_receipts` option returns 400. A checkpoint neither creates a private batch
nor waits for a private ACK; it is not the consumption-record transport.

Existing relay journals may contain inert `receipt_head:null` metadata. The
reader accepts it without changing events or balances. A non-null private head
is rejected explicitly; historical receipt rows are not silently discarded.

The independent verifier still accepts already-issued private proof bundles.
This preserves validation of historical bytes, not an active Router query or
private checkpoint ingestion path.

<a id="历史私有格式"></a>

### Historical private format

Fixed-size bytes are lowercase hex; monetary fields use no floating point.

```text
message    = UTF8("teerouter:consumption-batch:v1\0") || UTF8(manifest)
signature  = Ed25519.sign(process_private_key, message)
batch_hash = SHA256(message)
leaf       = SHA256(0x00 || random_32_byte_salt || UTF8(record.payload))
parent     = SHA256(0x01 || left_32_bytes || right_32_bytes)
empty_root = SHA256(empty_bytes)
```

Records are ordered by `seq`; odd final nodes duplicate at each tree level.
Inclusions contain leaf index and bottom-up sibling hashes. The signed count
determines depth and direction, and the record sequence must equal
`sequence_before + index + 1`. Historical manifests commit to ranges, counts,
priced count, calculated total, instance/boot and previous hash/boot.

Historical bundles contain `batch` and `identity`; optional `inclusions` contain
private `payload`, `salt`, `index` and `siblings`. These fields do not describe
current public batches. Already-signed historical bytes are never reformatted.

<a id="签名密钥证明"></a>

## Signing-key evidence

`POST /admin/receipts/evidence {"nonce":"<fresh 32-byte hex>"}` returns the
process public key, boot ID and hardware evidence bound to:

```text
REPORT_DATA = SHA512(UTF8("teerouter:receipt-key:v1\0") || nonce_32_bytes
                     || public_key_32_bytes || boot_id_16_bytes)
```

This endpoint does not attest a caller-supplied financial digest. Hardware
evidence uses the local guest interface; failure does not invoke a remote
attester or a software fallback. TLS channel binding is separate and unchanged.
Ordinary batches require only local Ed25519 signing.

The [independent verifier](../verifier/README.en.md) checks evidence against an
independently chosen policy. A valid signature does not establish hardware trust
by itself. Disclosure of entire checkpoints or account secrets is not required
to verify consumption evidence.
