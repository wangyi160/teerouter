<a id="tdx-测试证据"></a>

# TDX test evidence

[简体中文](README.md) | **English**

Public Intel-signed test evidence copied from the MIT-licensed `dcap-qvl` 0.6.5
crate, `sample/tdx_quote` and `sample/tdx_quote_collateral.json`.
Source: https://crates.io/crates/dcap-qvl/0.6.5
License: LICENSE.dcap-qvl in this directory.

SHA-256:

- tdx_quote: c42f9164325024bca2757bc8819b11879a0a369132ea4e2b7c85df4805ea72db
- tdx_quote_collateral.json: b0a5f5fd620a8881b1eda45261fdf30dd930b49aff93231556645c81fcb4c0bc

Offline tests use a historical clock inside this fixture's validity window.
This fixture provides historical cryptographic test input only, not a trusted
Router policy or current hardware evidence. Normal verification downloads current
collateral and uses the verifier's clock.
