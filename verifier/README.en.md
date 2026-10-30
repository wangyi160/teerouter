# teerouter-verify

[简体中文](README.md) | **English**

A standalone consumption-receipt verifier for Windows and Linux. It checks original batch signatures, record digests, ordering, amount totals and continuity. Intel TDX evidence and an independently selected reference policy can additionally establish the signing identity.

<a id="当前公开格式"></a>

## Current public format

Supports `teerouter.public-consumption-batch.v1`. `manifest` and `records_json` are verified as their original UTF-8 bytes and must not be reserialized. Public batches use a digest of all records and one signature per batch, without per-record Merkle proofs. User and key pseudonyms change between batches; refusals retain only their category.

Without `records_json`, verification covers only the signature and committed digest, not the record details themselves.

<a id="验证输入与结果"></a>

## Verification inputs and results

Supply the original evidence bundle and an independently trusted TDX policy:

```sh
teerouter-verify --bundle batch.json --policy trusted-tdx-policy.json
```

Success exits with code 0; failure exits with 1. JSON results separately report `tee_verified`, `signature_verified`, `records_verified` and `inclusions_verified`.

`tee_verified` is true only when evidence matches a locally and independently approved policy for the OS, Router compose and key provider. A public key or “verified” label within the evidence cannot serve as a reference policy. `policy/deny-all.json` trusts no evidence.

By default, verification fetches current Intel certificates, revocation and TCB materials and uses the local current time. Expired or revoked historical evidence may fail; a batch's self-reported time is not a trusted timestamp.

To check the mathematical signature only:

```sh
teerouter-verify --bundle batch.json --signature-only
```

This mode always returns `tee_verified:false` and does not establish a TEE identity. `tests/sample-bundle.json` uses a public test seed and is only a test vector.

<a id="历史私有格式与限制"></a>

## Historical private formats and limitations

The verifier retains support for previously issued private batches, including randomly salted payloads, Merkle paths and historical empty batches. These are not the current Router's receipt-query or upload interfaces. See the [consumption batch protocol](../docs/CONSUMPTION-PROOFS.en.md#historical-private-format).

- Signatures establish the provenance and integrity of supplied bytes, not complete, undisclosed-free or necessarily current history. `complete_history` and `latest_verified` are always false.
- A preceding signed batch may be supplied to verify continuity. If it uses a different public key, that identity still requires independent verification; `previous_tee_verified:false` makes this limitation explicit.
- Abnormal Router exits can lose uncommitted records. Signatures and restart identifiers cannot recreate missing records.
- Consumption proofs do not cover external balance changes or guarantee complete official usage for every network failure.

<a id="本地构建与测试"></a>

## Local builds and tests

```sh
cargo build --release --locked --manifest-path verifier/Cargo.toml
cargo test --locked --manifest-path verifier/Cargo.toml
```

TDX verification checks OS/RTMR values, complete event replay, Intel trust roots and TCB. See [test evidence](tests/fixtures/tdx/README.en.md) for sources, SHA-256 digests and licenses.
