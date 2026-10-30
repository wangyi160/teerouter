# teerouter

[简体中文](README.md) | **English**

A Rust LLM router that runs inside a trusted execution environment (TEE). This repository documents the Router's code structure, HTTP interfaces, native forwarding, authentication, billing, state synchronization and verification.

<a id="核心能力"></a>

## Core capabilities

- Supports OpenAI / ChatGPT, Claude / Anthropic and Grok / xAI, connecting to fixed official HTTPS domains through each account's configured proxy.
- Forwards Chat Completions, Responses and Messages in their native formats, preserving caller-supplied generation parameters. After resolving a routing model identifier, it sends the corresponding official model ID upstream.
- Shares official model catalogs by provider while checking account credentials, health, limits, pool allowlists and user permissions independently. New requests requiring an invalid catalog are rejected.
- Isolates all upstream accounts in merchant runtimes. User requests require both Bearer authorization and a client Ed25519 signature; account secrets are accepted only through the owner-authenticated merchant channel.
- Supports fixed pools or automatic routing, multiplier ceilings, price or proxy-latency priority, and session affinity. All API keys belonging to a user share a concurrency gate. Insufficient balance or exhausted queue capacity stops new requests.
- Reads usage from native responses and calculates charges using provider-specific rules and a quote frozen at request admission. Interrupted requests are classified using observed transmission progress and usage.
- Groups consumption records into Ed25519-signed batches, with separate upload and settlement acknowledgments. An optional authenticated, encrypted journal recovers committed accounting records.
- Implements TLS 1.3, challenge-bound dstack TDX / Linux SNP evidence collection and model forwarding in one process. A standalone consumption-receipt verifier is also included.

`GET /admin/public-catalog` returns sanitized summaries of current pools, models, prices, capacity and available accounts. This read-only control endpoint neither refreshes catalogs nor establishes customer relationships. It excludes credentials, proxy addresses and raw errors. See the [API reference](docs/API.en.md) for authentication and request formats.

<a id="代码与示例"></a>

## Code and examples

| Path | Contents |
| --- | --- |
| `src/`, `tests/` | Router implementation and tests |
| `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | Rust build configuration and pinned dependencies |
| `config*.example.toml`, `provision*.example.json` | Internal engine protocol test fixtures |
| `protocol-update.example.json`, `restrictions-update.example.json` | Configuration structure examples |
| `egress/` | Source for the VLESS egress helper process |
| `verifier/` | Standalone consumption-receipt verifier |
| `docs/` | Router interfaces, protocols and data boundaries |

TOML and local credential-file assembly are used only by `cfg(test)` fixtures. Root runtime initialization rejects upstream account pools. Merchant accounts and public policies use separate authenticated channels; see the [merchant protocol](docs/MERCHANTS.en.md).

<a id="本地构建与测试"></a>

## Local builds and tests

The crate and executable are named `teellm`.

```sh
cargo check --locked --lib --bins --no-default-features
cargo check --locked --lib --bins --no-default-features --features dstack-tdx
cargo test --locked --all-targets --features sample-demo
```

`sample-demo` is disabled by default and intended only for controlled tests. It does not produce hardware evidence accepted by strict verifiers. Local tests use mock upstreams; they neither establish that real accounts work nor replace hardware attestation verification. The journal protects only committed records; see [Accounting journal](docs/ACCOUNTING-JOURNAL.en.md).

<a id="文档索引"></a>

## Documentation index

- [Code structure](ARCHITECTURE.en.md), [State and synchronization](OPERATIONS.en.md), [HTTP API](docs/API.en.md), [TEE security boundaries](SECURITY-TEE.en.md).
- [User request signatures](docs/USER-AUTH.en.md), [Merchant authentication and configuration](docs/MERCHANTS.en.md), [Account pools](docs/MERCHANT-POOLS.en.md), [VLESS egress](docs/VLESS.en.md).
- [Billing](docs/BILLING.en.md), [Interrupted-request billing](docs/INTERRUPTED-BILLING.en.md), [Price sources](docs/PRICE_SOURCE.en.md), [Merchant accounting boundaries](docs/MERCHANT-LEDGER.en.md).
- [Signed consumption batches](docs/CONSUMPTION-PROOFS.en.md), [Local accounting journal](docs/ACCOUNTING-JOURNAL.en.md), [Standalone verifier](verifier/README.en.md).

The code includes selected LiteLLM ports and new Rust integration. See [NOTICE](NOTICE) and [UPSTREAM.json](UPSTREAM.json) for provenance and modifications. Licensed under MIT; dependencies retain their respective licenses.
