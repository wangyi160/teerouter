<a id="客户端持有密钥的用户请求签名-v2"></a>

# Client-owned user request signatures v2

[简体中文](USER-AUTH.md) | **English**

A valid user Bearer grants permissions but is insufficient for model requests.
Router also verifies an Ed25519 signature using the registered `User.public_key`.
`User.id` must be `u_sig_` followed by lowercase SHA-256 hex of that 32-byte public
key. Configuration writes validate this derivation; changing the key for the same
ID fails validation. Router receives no user signing seed.

<a id="线上格式"></a>

## Wire format

Required single-valued headers:

- `x-router-user-id`: registered identity; the old public-key header is rejected.
- `x-router-user-timestamp`: canonical decimal Unix seconds.
- `x-router-user-nonce`: fresh 32-byte random value, lowercase hex.
- `x-router-user-signature`: 64-byte Ed25519 signature, lowercase hex.

Sign the UTF-8 compact JSON array:

```text
["teerouter-user-request-v2", boot_id, user_id, method, path_and_query,
 timestamp_string, nonce_hex, sha256_body_hex, [[header_name, value_bytes_hex], ...]]
```

Header names are lowercase and sorted; repeated values retain wire order. All
headers are covered except `x-router-user-*` and the transport fields `host`,
`content-length`, `connection`, `transfer-encoding`, `keep-alive`, `te`, `trailer`,
`upgrade`, `proxy-authorization`, `proxy-connection`. `Connection` may contain only
`close` or `keep-alive`; it cannot nominate another field for removal.
Authorization, agent identity and routing headers are signed. Body hashing uses
the exact bytes received by Router before JSON parsing, not reserialized JSON.

<a id="验证与重放保护"></a>

## Verification and replay protection

The proof binds the current Router `boot_id` obtained over the independently
verified TLS channel. Proofs expire after 300 seconds; future clock skew is at
most 60 seconds. Successful verification consumes the nonce in a bounded,
process-wide cache shared across configuration changes. Saturation rejects new
proofs. A new boot invalidates all earlier proofs.

Signature failures and replays return 401 before routing or billing. Signed
requests are not automatically replayed after sending begins. The user ID also
keys concurrency, merchant customer limits, refusal counters and consumption;
Router maintains no secondary customer ID or identity alias.

Handshake JWTs authorize only evidence/health negotiation. They do not authorize
model or control operations. Administrator authority can change permissions,
balances or service admission, but cannot substitute for a valid user signature.
This guarantee assumes the verified Router implementation is the one executing.
