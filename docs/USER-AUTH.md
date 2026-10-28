# Client-owned user request signatures v2

A management-issued Bearer grants permissions but is insufficient for model requests. teeproxy generates its own Ed25519 key; the management platform receives only the public key and an enrollment proof. The Router identity is `u_sig_` followed by lowercase hex SHA-256 of the 32-byte public key. Management synchronizes `User.id` and `User.public_key` once during registration/configuration. Router validates the ID-to-public-key derivation on configuration writes, then looks up the registered public key for each signed request. Replacing the key for the same ID fails validation.

## Wire format

Required single-valued headers:

- `x-router-user-id`: registered `u_sig_` identity. Public keys are not sent in model requests; the old public-key header is rejected.
- `x-router-user-timestamp`: canonical decimal Unix seconds.
- `x-router-user-nonce`: fresh 32-byte random value, lowercase hex.
- `x-router-user-signature`: 64-byte Ed25519 signature, lowercase hex.

Sign the UTF-8 compact JSON array:

```text
["teerouter-user-request-v2", boot_id, user_id, method, path_and_query,
 timestamp_string, nonce_hex, sha256_body_hex, [[header_name, value_bytes_hex], ...]]
```

Header names are lowercase and sorted; multiple values retain their wire order. All headers are covered except the proof headers (`x-router-user-*`) and transport fields: `host`, `content-length`, `connection`, `transfer-encoding`, `keep-alive`, `te`, `trailer`, `upgrade`, `proxy-authorization`, `proxy-connection`. A `Connection` header may only contain `close` or `keep-alive`; it cannot nominate another field for removal. Authorization, agent identity and routing headers are covered. Body hashing uses the exact bytes delivered to Router, before JSON parsing. There is no body reserialization in teeproxy.

The boot identity is read on the same independently attested TLS session. Proofs expire after 300 seconds with at most 60 seconds of future clock skew. A bounded, process-wide replay cache consumes a nonce after successful verification and survives configuration changes. Cache saturation fails closed. A new Router boot invalidates all earlier proofs. Signature failure and replay return 401 before upstream routing or billing; requests are never automatically replayed after sending begins.

## Enrollment and migration

The management connection API accepts a public key plus an Ed25519 proof over compact JSON `["teerouter-user-enrollment-v1", platform_account_id, device_id, public_key_hex]`. Existing bindings cannot be silently overwritten. Private seeds stay in the client's portable encrypted data directory. Registration and login use a one-time Ed25519 challenge, with no user password. The challenge binds the platform origin, purpose, role, public key, nonce and expiry. User/merchant IDs and their private keys are retained locally across logout; losing the private key loses access to that identity.

The same `u_sig_…` ID identifies the user in the platform wallet, Router configuration, merchant customer restrictions, refusal counters and billing. Router has no secondary customer ID or ID alias. Merchant identities similarly use `m_sig_` plus SHA-256 of their 32-byte public key. Migration of pre-existing financial references belongs to the management platform; historical signed receipts remain byte-for-byte unchanged and are resolved during archival ingestion.

TEE evidence and health negotiation still use the narrowly scoped login JWT, before model traffic. Admin authority can grant/revoke access, manage balances and interrupt service; this protocol does not remove that authority or protect against a malicious replacement of the trusted Router code. It prevents the platform's possession of the Bearer alone from producing requests signed as the client's identity.
