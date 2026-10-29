# Account-bound VLESS egress

An account's `proxy` field accepts a VLESS share URI through the owner-authenticated
account synchronization channel. Example (dummy credentials only):

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

Account bindings apply to model discovery, inference/streaming and credential probes.
Router does not perform OAuth renewal.
Official upstream hosts and their HTTPS certificate checks remain enforced by
the router. A failed VLESS route never falls back to direct access or the system
proxy. Target hostnames pass through SOCKS to VLESS; only the node itself is
resolved locally. Inner provider TLS terminates at the provider, not at Xray.

The router uses `teerouter-egress`, a private Go helper embedding pinned
Xray-core. Rust starts one process on demand. It exposes only an authenticated
loopback SOCKS listener, with an unadvertised random password; its configuration
channel is stdin/stdout, not a network API. UUIDs/configuration remain in memory
and private pipes, never CLI arguments or files. The helper exposes no public configuration endpoint.

Identical node configurations share a route across accounts. Routes are retained
by live HTTP clients and drain ongoing streams when their clients are replaced.
There are at most 64 active/draining node routes and 2048 concurrent SOCKS sockets
per router, bounded by the runtime's memory/process limits. Admission fails
closed when capacity is exhausted. Helper exit causes current requests to fail;
the next newly constructed client can start a fresh helper. Private account bindings are reconstructed by the owner-authenticated account
synchronization protocol; root checkpoints do not contain those secrets.

For local source checks in `egress/`, use the Go version declared in `go.mod`:

```sh
go build -mod=readonly .
go test ./...
```

Tests cover local TLS/XHTTP, authentication, fail-closed behavior, stream draining
and cancellation. Module versions are locked in `go.mod`
and `go.sum`; dependency license requirements are recorded in the repository's
`NOTICE`. The helper is part of the code covered by the TEE trust policy.
