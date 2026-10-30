<a id="本地流水日志"></a>

# Local accounting journal

[简体中文](ACCOUNTING-JOURNAL.md) | **English**

Router's optional accounting journal persists committed consumption and delivery state using SQLite WAL and authenticated encryption. Without it, that state lives only in memory. Initialization requires local dstack key derivation; an unavailable key, failed authentication or corrupt journal rejects initialization instead of silently creating a replacement ledger.

<a id="提交与上传"></a>

## Commit and upload

- After a completed, failed or cancelled request produces an accounting event, changed rows are committed with `synchronous=FULL` transactions. The database is not rewritten for each request.
- The journal stores records, cumulative usage, synchronized balances, unsettled consumption, upload cursors and frozen batches. It excludes request bodies, answers, upstream credentials and TLS private keys. Public-mode refusals contain only their category.
- `interval_seconds` defaults to 300. A failure ends the current cycle and is retried in the next one; idle cycles retain the scheduling interval.
- Each batch contains at most 512 records and is limited to approximately 900 KB. A cycle processes its existing backlog and stops on any failed batch; newly arriving events cannot extend it indefinitely.
- Frozen batches, nonces, signatures and scheduling times are persisted before network requests begin. Lost acknowledgments or restarts resend exactly the same signed bytes.
- A validated durable-upload ACK releases the corresponding details, clears the frozen batch and advances the upload cursor in one transaction. It does not clear consumption.
- A settlement ACK separately updates synchronized balances and clears consumption within the acknowledged boundary. Duplicate acknowledgments are not applied twice; later requests remain.

<a id="恢复"></a>

## Recovery

A new process has a new `boot_id` and signing key. During business initialization it checks the journal's application identity, sequences and acknowledgment boundaries. Historical records are restored using the balance/consumption combination recorded in the journal; later balances are applied through settlement.

If the remote service saved a batch but its upload ACK is missing locally, the original batch remains. Router accepts settlement through that boundary only after a repeated upload obtains a valid ACK. Until then it uses the old local balance minus consumption, avoiding subtraction of old consumption from a newly settled balance.

The journal protects successfully committed records only. It cannot recreate calls unfinished at the time of a crash or official usage never observed. In-flight tails that were not generated, not committed or failed to write are outside recovery guarantees. Entire-journal rollback, deletion or storage failure also preclude a guarantee of lossless recovery.

<a id="加密容量与状态"></a>

## Encryption, capacity and status

The AES-256-GCM key is derived locally through dstack at `teerouter/accounting-journal/v1`. Each row uses an independent random nonce and its row name as authenticated context. An additional encrypted digest authenticates the complete row set to detect row deletion, replacement or relocation. Externally supplied keys are not accepted.

An exclusive database connection rejects a second writer process. The logical journal limit is 64 MiB and the database limit is 256 MiB, with periodic WAL checkpoints. In-memory queue and unsettled-batch limits also apply. Reaching a limit or failing a write closes admission for new model requests, preserves committed data and does not fall back to memory-only accounting.

`/admin/status.accounting_journal` and `/admin/metrics.storage.journal` report whether journaling is enabled and healthy. Implementation: `src/vault/journal.rs`; commit, acknowledgment and recovery regression tests: `src/vault/journal/tests.rs`.
