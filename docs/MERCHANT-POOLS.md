# Merchant pool publication

The platform command `PUT /admin/merchants/{id}/config` replaces the merchant's published pool set. Pools with no
deployments are omitted, together with their `pool_pricing` entries. Removing the
last account withdraws the pool; a disabled account still counts as an account.
Other merchants are unaffected. Clients retain empty drafts and their settings
locally instead of replacing their complete local account book with this response.

`POST /merchant/accounts/test` can test the first account before its pool is
published. Include the normal `pool`, `proxy`, and access-only `credential` (or
`api_key`), plus:

```json
{
  "draft_pool": {
    "provider": "openai",
    "allowed_models": ["model-id"]
  }
}
```

`allowed_models` may be null for all available text models, or an empty list to
allow none. The draft is accepted only if the named pool is absent from this
merchant's runtime. Published pools use their existing provider and model policy
and reject a draft override. The same official endpoints, proxy validation,
merchant authorization, Workbuddy test permission, and probe limits apply.
Testing does not publish the pool, persist credentials, refresh tokens, or create
user charges. Save the account locally, then publish the nonempty pool after a
successful test.
