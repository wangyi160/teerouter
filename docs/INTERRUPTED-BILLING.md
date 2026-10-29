# Interrupted request billing

An interruption is not evidence of zero upstream work. Billing uses the frozen
quote for each attempt and records a deterministic disposition from observable
progress. The policy does not claim to know whether the remote application ran.

| Outcome | Settlement |
| --- | --- |
| Router rejects admission, validation or authorization | Zero |
| Upstream rejects the request with HTTP 3xx/4xx | Zero; counters, if present, retained as `waived_usage` |
| HTTP 5xx with no generation/usage evidence | Zero |
| Connection fails, or request is cancelled before complete local transmission, with no generation/usage evidence | Zero |
| Request body is completely written and flushed, then connection fails/times out/is cancelled before a reply | Estimate input at ordinary input price; zero observed output |
| Upstream has accepted the request (2xx headers), then reading fails | Official counters where available; otherwise estimate input and observed output |
| Failure after partial generation | Official counters where available; estimate missing input and observed output |
| Complete official usage on an executed request, even if delivery fails | Official usage, including confirmed cache discount |
| Successful response with missing usage, invalid counters or unsupported estimation | Keep explicit pending reason; never manufacture official usage |

HTTP request rejection differs from a generated refusal inside a successful model
response. Generated refusals remain model output and follow normal usage billing.
A 5xx with valid usage is an execution failure rather than admission rejection.

Each attempt records `transport.version=1`, `transport.phase`,
`transport.request_flushed`, `termination`, `upstream_status` and its billing
reason/source. Phases are `not_started`, `connecting`, `writing_request`,
`awaiting_response`, `response_headers`, and `receiving_response`.

`request_flushed` is set only after the serialized HTTP headers and full body
have been accepted by the authenticated connection and its flush succeeds.
Handing a body to an HTTP library, opening a proxy tunnel or completing TLS does
not set this flag. Flush is a local boundary, **not proof of remote receipt or
execution**. TCP/TLS cannot eliminate this distributed-systems ambiguity; the
above policy fixes the financial disposition without pretending otherwise.

Inference uses HTTP/1 with bounded connection reuse and explicit HTTP(S), SOCKS5h
or VLESS account egress. TLS certificate verification stays enabled. Every retry
gets fresh observations. The HTTP transport never retries a transmitted model
request internally; router retries are separately evidenced and charged once.

When a downstream request/response task is dropped, Router cancels its upstream
operation, releases request permits and records 499 (`request_cancelled`). It
does not drain a model reply in the background or issue another attempt. Dropping
the metered response also aborts its connection driver. This cannot guarantee
that an upstream application instantly stops work on noticing a closed socket.
The already observed progress determines the charge; unobserved future output is
not estimated. 499 identifies task cancellation, not which physical peer caused
it, and never proves an explicit user action.

Only interrupted/failed model requests may use local estimates. They remain
labelled `estimated`/`mixed`, `coverage=partial`; hidden output is not invented.
The actual serialized native provider request supplies the input estimate. Text remains transient and is excluded from signed receipts.
Existing signed receipts are not rewritten under this new policy.
