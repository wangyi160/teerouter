# Merchant accounting ownership

The platform ledger records each routed request once, including the merchant ID,
request-time pricing, usage, calculated charge and refusal category. Its accounting
journal and signed batch publisher remain responsible for durable export.

Merchant runtimes retain per-customer usage totals for limits and the existing
upstream-user relationship. They no longer create a second usage/refusal outbox.
Merchant availability therefore does not depend on a merchant client downloading
or acknowledging receipts. The platform ledger's capacity and durability checks
still apply to request admission.

Merchant applications obtain ledger records and settlement updates from their management platform. Legacy direct ledger query/ACK endpoints have been removed. Account and access-token uploads remain direct to the attested Router; public configuration is platform-mediated.
