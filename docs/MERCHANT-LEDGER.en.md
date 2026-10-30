<a id="router-内部的商户记账边界"></a>

# Merchant accounting boundaries inside Router

[简体中文](MERCHANT-LEDGER.md) | **English**

The root runtime creates one consumption event for each routed request, associating the merchant, user, pool, model, quote frozen at admission, usage, amount and refusal category. The shared accounting journal and signed-batch publisher process this event.

Merchant runtimes retain customer usage aggregates, limits and visibility relationships established by receiving an upstream response. They do not maintain a second pending consumption or refusal queue. Merchant disconnection or failure to read state therefore creates no second backlog of details and does not determine model-request admission.

Root accounting queue capacity, unsettled consumption, upload acknowledgments and journal health still constrain new requests. Upload ACKs release only uploaded details. Settlement ACKs use a separate endpoint to update synchronized balances and consumption boundaries. See [Synchronization](../OPERATIONS.en.md), [Signed batches](CONSUMPTION-PROOFS.en.md) and [Local journal](ACCOUNTING-JOURNAL.en.md).

Router provides no merchant-specific per-record ledger query or ACK endpoint. Usage, customer relationships and refusal counters in public merchant state support limits and authorization; they are not an additional ledger.
