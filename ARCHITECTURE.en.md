<a id="router-代码结构"></a>

# Router code structure

[简体中文](ARCHITECTURE.md) | **English**

```text
HTTP request → Authentication and user signature → Permissions, balance, rate limits → Merchant/account routing → Native upstream request
                                                                                                              ↓
Response or cancellation → Usage and interruption classification → Charge event → Local journal → Signed batch and upload ACK
                                                                                       ↑
                                                                   Control API: configuration, recovery, settlement
```

<a id="请求与账号隔离"></a>

## Request and account isolation

`src/server.rs` defines HTTP routes and authentication boundaries; `src/user_auth.rs` verifies user signatures and maintains the replay cache. Handshake JWTs, user Bearers, administrator Bearers and merchant owner credentials grant different permissions.

The root runtime holds user permissions, public merchant registrations, aggregate usage and consumption. Upstream accounts and pools exist only in merchant runtimes. Root configuration rejects account pools, account credentials and egress proxies. `src/merchants/` handles merchant identity, public policies, private account synchronization and cross-merchant routing. Public control endpoints cannot export merchant secrets.

`src/router.rs` and `src/merchants/routing.rs` enforce permissions, model capabilities, concurrency, multiplier and session constraints. Hot configuration updates preserve user concurrency gates; deleting and recreating a user does not bypass slots held by in-flight requests. Lower limits affect only subsequent admission.

`src/client.rs` and `src/upstream.rs` implement native forwarding only; `src/sse.rs` handles frame boundaries. Requests and responses are not converted between Chat and Claude formats. `src/discovery/` fetches and validates official catalogs. `src/network/` and `src/official.rs` enforce account proxies and official-domain restrictions.

<a id="计费与状态"></a>

## Billing and state

`src/accounting.rs` manages request and stream lifecycles. `src/ledger.rs` records usage, charges and refusal categories. `src/billing/` calculates charges using a quote frozen when the request starts and the respective OpenAI, Anthropic or xAI metering semantics.

`src/balances.rs` checks available funds as synchronized balance minus unsettled consumption. Requests accumulate consumption without directly rewriting the synchronized balance. `src/receipt_relay.rs` separates upload and settlement acknowledgments. Settlement clears only consumption within the acknowledged boundary, preserving later requests. See the [billing protocol](docs/BILLING.en.md).

`src/vault.rs` holds mutex-protected in-memory state; `src/vault/operations.rs` handles revision comparisons, atomic updates, credential races and short-lived sessions. Optional `src/vault/journal.rs` persists accounting state in SQLite WAL/FULL transactions. It authenticates and encrypts each row and authenticates the complete row set, without storing upstream credentials or request bodies.

`src/refusal.rs` collects explicit native refusal signals. Public consumption batches retain only refusal categories; merchant refusal counts accumulate separately. Historical refusal queues are still structurally validated on recovery, but the normal relay flow does not append queues of refusal text.

<a id="生命周期与证明"></a>

## Lifecycle and attestation

`src/runtime.rs` generates a new `boot_id` on each process start. Initialization restores the logical instance, configuration and accounting state, but not official catalog evidence, health or short-lived sessions. Commands for an old process cannot acknowledge a new process's state.

There is no control-connection heartbeat lease after initialization. Disconnection alone does not stop business traffic; balance, queue capacity and journal-health constraints remain effective. Root and merchant runtimes share an explicit drain flag and background-task counts. Only `/admin/quiesce` changes the flag, which is not restored from checkpoints.

`src/attestation.rs` binds the TLS certificate, random challenge and TEE evidence within the same process. The per-process signing key in `src/receipts.rs` is bound to public consumption batches through a separate REPORT_DATA construction. See [TEE security boundaries](SECURITY-TEE.en.md).

File, environment-variable and TOML test assembly lives in `tests/config_fixture.rs` and is excluded from normal builds. Root initialization and merchant synchronization validate inputs against their defined protocols.
