# Rust real-runtime E2E tests

This independent crate contains sandbox SDK live scenarios and local test-fixture
checks. Live sandbox scenarios are ignored by default; ordinary `cargo test` does
not allocate remote sandboxes.

From this directory, check the local fixtures with `cargo test --locked`.
For runtime setup, commands, prerequisites, logging and validation scope, use:

- [UID/GID, argv and Redis/TLS tests](../../docs/sdks/rust.md#live-identity-argv-and-redis-tests)
- [Full Docker and existing-server tests](../../docs/sdks/rust.md#full-runtime-tests)
- [Kubernetes and VM tests](../../docs/sdks/rust.md#kubernetes-and-vm-tests)
- [Interpreter tests and diagnostics](../../docs/sdks/rust.md#interpreter-tests)
- [Recorded live results and remaining VM validation](../../docs/sdks/rust.md#recorded-live-results)

Test coverage and HTTP fixture checks do not establish live-runtime acceptance.
