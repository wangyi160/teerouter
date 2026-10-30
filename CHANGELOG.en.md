<a id="更新记录"></a>

# Changelog

[简体中文](CHANGELOG.md) | **English**

<a id="未发布"></a>

## Unreleased

- Raised the default model-request limit to 512 MiB to cover the published OpenAI image-request range; local size violations, read failures and upstream 413 responses are reported separately.
- A late 401 from an old credential no longer disables its replacement; removed interference from old signed snapshots with the current receipt synchronization flow.
- Focused documentation on Router code, interfaces, state and verification constraints.
- Removed obsolete protocol conversion, configuration scanning, recovery capsules and migration logic used only by old tests.
- Moved local configuration import into test fixtures and updated related assertions to use native forwarding and current control interfaces.

## 0.1.0

Standalone Rust Router: native model forwarding, in-memory state, control interfaces, TEE evidence collection and local tests.
