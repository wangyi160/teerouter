# Account-bound VLESS egress

An account's existing `proxy` field accepts a VLESS share URI. The merchant
workbench accepts the same URI when adding/editing an account or importing a
batch. Example (dummy credentials only):

```json
{"proxy":"vless://12345678-1234-1234-1234-123456789abc@node.example:443?encryption=none&security=tls&type=xhttp&sni=node.example&path=%2Ftunnel&mode=stream-one"}
```

Supported: TLS with TCP or XHTTP (`auto`, `stream-one`, `stream-up`, `packet-up`).
Optional fields: `sni`, `fp` (chrome/firefox/safari/edge/randomized), `alpn`
(h2/http/1.1), XHTTP `host`/`path`/`mode`, TCP `flow=xtls-rprx-vision`.
Only UUID identities and `encryption=none` are accepted. REALITY, WS, gRPC,
XHTTP extra JSON, insecure certificates and unrecognized/duplicate parameters
are rejected. An IPv6 node requires a DNS `sni`. Certificate verification is
mandatory; a public CA certificate for the node's SNI must be valid.

Account bindings apply to model discovery, inference/streaming and OAuth refresh.
Official upstream hosts and their HTTPS certificate checks remain enforced by
the router. A failed VLESS route never falls back to direct access or the system
proxy. Target hostnames pass through SOCKS to VLESS; only the node itself is
resolved locally. Inner provider TLS terminates at the provider, not at Xray.

The container includes `teerouter-egress`, a private Go helper embedding pinned
Xray-core. Rust starts one process on demand. It exposes only an authenticated
loopback SOCKS listener, with an unadvertised random password; its configuration
channel is stdin/stdout, not a network API. UUIDs/configuration remain in memory
and private pipes, never CLI arguments or files. No extra published port, host
network, TUN interface, capability or persistent volume is required.

Identical node configurations share a route across accounts. Routes are retained
by live HTTP clients and drain ongoing streams when their clients are replaced.
There are at most 64 active/draining node routes and 2048 concurrent SOCKS sockets
per router, bounded by the existing container memory/PID limits. Admission fails
closed when capacity is exhausted. Helper exit causes current requests to fail;
the next newly constructed client can start a fresh helper. Configuration is
restored through the normal encrypted management snapshot/account provisioning.

For local builds, build `egress/` with Go 1.27.2 (`go build -mod=readonly -o
../target/debug/teerouter-egress .`; append `.exe` on Windows). Place the helper
beside `teellm`, or set the operator-only `TEEROUTER_EGRESS_BIN` to its path.
Run `go test ./...` in `egress/` for local TLS/XHTTP, authentication, fail-closed,
stream-draining and cancellation tests. The production Dockerfile pins the
toolchain checksum, module versions and licenses; changing the image changes
the TEE measurement and requires the normal reviewed trust-policy update.
