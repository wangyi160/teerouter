# Merchant pool state

The owner-authenticated `PUT /merchant/accounts/sync` creates or updates account
bindings. A new nonempty pool can include initial `new_pool_pricing`. Existing
pool policy and multipliers are preserved; public policy updates use
`PUT /admin/merchants/{mid}/config` with admin authentication.

Pools with no accounts are omitted, together with their `pool_pricing` entries.
Removing the last account withdraws a pool; a disabled account still counts as
an account. Other merchants are unaffected. An empty draft is not a live pool.

`POST /merchant/accounts/test` can test an account before its pool exists.
Include `pool`, `proxy`, access-only `credential` (or `api_key`), plus:

```json
{"draft_pool":{"provider":"openai","allowed_models":["model-id"]}}
```

`allowed_models:null` permits all officially available text models; `[]` permits
none. A draft is accepted only if the named pool is absent from this merchant's
runtime. Existing pools use their current provider and model policy and reject a
draft override. Official endpoint, proxy, authorization and probe limits apply.

Testing does not create a pool, persist credentials, renew tokens or create user
consumption records. Account synchronization and testing are separate operations.
