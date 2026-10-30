<a id="tee-安全边界"></a>

# TEE security boundaries

[简体中文](SECURITY-TEE.md) | **English**

Router implements TLS 1.3, model forwarding, dstack Intel TDX and Linux SNP evidence collection. This document describes the identities verified by the code, its data boundaries and attestation limitations.

<a id="密钥与权限"></a>

## Keys and permissions

TLS private keys and per-boot consumption-signing keys are generated inside Router and have no export API. Merchant accounts, access tokens and proxy secrets are used only in their respective runtimes; root control interfaces expose sanitized state. Administrator authority, merchant control keys and user signing keys are not interchangeable.

User requests require both a valid Bearer and a client Ed25519 signature. Bearer possession or administrator authority cannot forge that signature; administrators can still change permissions, balances and service admission. This property assumes the executing Router matches the code approved by the verifier.

The optional accounting journal authenticates and encrypts each row using a key derived through local dstack `GetKey`. It does not accept externally supplied journal keys or store model bodies, upstream account credentials or TLS private keys.

<a id="通道证明"></a>

## Channel attestation

The random challenge and current TLS certificate are bound into REPORT_DATA using:

```text
SHA512(UTF8("teellm:attested-channel:v1\0") || nonce32 || SHA256(certDER))
```

`dstack-tdx` obtains a TDX quote through the local guest interface. Verifiers check Intel certificate chains, revocation and TCB, MRTD, RTMR0–2, the complete RTMR3 event sequence and REPORT_DATA. Evidence and reference policies must be independent: network-supplied materials alone cannot establish trust.

The Linux SNP path collects evidence through `/dev/sev-guest` and returns the CoCo SNP structure. Reports must be verified against AMD certificate chains, signatures, freshness, versions, TCB, DEBUG, VMPL and independent reference values. See the standalone receipt verifier's [documentation](verifier/README.en.md) for its supported evidence types.

An unavailable evidence interface returns `attestation_unavailable`, without falling back to software claims. `sample-demo` is an explicit test feature; its sample-marked evidence is rejected by strict verifiers.

A hardware signature alone cannot establish trust in arbitrary code or files. That conclusion requires joint verification of the complete measurement chain, approved reference values and binding to the current connection. Local unit tests do not constitute real hardware attestation.

<a id="内容与上游"></a>

## Content and upstream services

Provider domains are fixed in code. Accounts have explicit proxy bindings, with no direct-connection fallback on failure; Router still validates official HTTPS certificates. Official catalogs and capabilities come from authenticated upstream responses. Control inputs can restrict model permissions, but cannot inject official catalog evidence.

Native forwarding preserves messages, instructions, tools, attachments and generation parameters. Resolving a routing identifier does not change the requested model's semantics. Configuration cannot inject generation bodies, rewrite reasoning parameters or replace protocols. `store` retains the caller's value; Router makes no promise about the upstream provider's internal retention behavior.

Explicit refusal/content_filter signals classify refusals. Public batches contain only categories and accounting metadata, not inputs, answers, refusal text or tool contents. Classification is not a legal or compliance judgment about user behavior.

<a id="状态完整性与限制"></a>

## State integrity and limitations

Root checkpoints contain public configuration, user permissions, public merchant registrations, cumulative usage and relay state, excluding merchant secrets. Official catalogs, health and session bindings are rebuilt in a new process. Reading a checkpoint does not settle consumption; settlement separately validates the revision, boot identifier and uploaded boundary.

The local journal validates individual rows and the complete row set, rejecting unauthenticated replacement, deletion and insertion. Frozen batches retain their original signatures; new batches use the current process key. Recovery also checks acknowledgment boundaries. This mechanism cannot prevent rollback or deletion of an entire journal copy, or recover uncommitted in-flight records.

Unsynchronized usage can be lost without a journal. Journal write failures close new admission, but the preceding uncommitted tail remains outside recovery guarantees. Consumption signatures establish provenance and integrity, not complete, undisclosed-free or necessarily current history. See [Consumption batches](docs/CONSUMPTION-PROOFS.en.md) and [Local journal](docs/ACCOUNTING-JOURNAL.en.md).
