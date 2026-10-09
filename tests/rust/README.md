# OpenSandbox Rust E2E tests

Run the default Docker-backed workflow from the repository root:

```bash
bash scripts/rust-e2e.sh
```

To use an existing server instead, set the following environment variables:

- `OPENSANDBOX_TEST_DOMAIN`: required `host[:port]` or full URL
- `OPENSANDBOX_TEST_PROTOCOL`: `http` (default) or `https`
- `OPENSANDBOX_TEST_API_KEY`: server API key, empty by default
- `OPENSANDBOX_SANDBOX_DEFAULT_IMAGE`: image supporting execd bootstrap
- `OPENSANDBOX_TEST_USE_SERVER_PROXY=true`: route data-plane calls through the server
- `OPENSANDBOX_TEST_NETWORK_POLICY=true`: enable and verify a policy sidecar
- `OPENSANDBOX_TEST_SECURE_ACCESS=true`: test secured endpoints on a supporting
  Kubernetes runtime, not Docker

```bash
cargo test --manifest-path tests/rust/Cargo.toml --locked \
  sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture
```

The default test verifies command output, text/binary file roundtrips, metrics,
and explicit deletion. Every scenario attempts deletion even when a workload
step fails. Only sandboxes allocated by these tests are destroyed.

Source-specific tests require fixtures on a supporting runtime. They are not
part of the default Docker workflow:

```bash
# Ready snapshot ID, reusable by this test
export OPENSANDBOX_TEST_SNAPSHOT_ID=your-ready-snapshot
cargo test --manifest-path tests/rust/Cargo.toml --locked snapshot_source -- --ignored --nocapture

# Succeeded template ID accessible to the current tenant
export OPENSANDBOX_TEST_TEMPLATE_ID=your-ready-template
cargo test --manifest-path tests/rust/Cargo.toml --locked template_source -- --ignored --nocapture
```

These tests create new sandboxes from the fixtures and delete those sandboxes,
not the source snapshots or templates. Missing fixture IDs fail explicitly.
