# OpenSandbox Rust SDK

Async Rust client for OpenSandbox lifecycle, execd, network policy, Credential
Vault, isolated sessions, diagnostics, and client-side pools.
Requires Rust **1.85+** and a Tokio runtime. This SDK is available from source and
has **not been published to crates.io**.

## Install from source

From this repository, add a path dependency to your application's `Cargo.toml`:

```toml
[dependencies]
opensandbox = { path = "../OpenSandbox/sdks/sandbox/rust" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

For a Git dependency, specify this repository and pin a revision containing the
Rust SDK. Cargo locates the `opensandbox` package within the repository.

## Quick start

Start an OpenSandbox server, then run the packaged example:

```bash
export OPEN_SANDBOX_DOMAIN=localhost:8080
export OPEN_SANDBOX_API_KEY=your-api-key
cargo run --locked --example quickstart
```

The [example](examples/quickstart.rs) creates a sandbox, executes a command,
writes and reads a file, and explicitly destroys the sandbox even if the
workload fails. Dropping a client **does not delete the remote sandbox**.

For configuration, streaming, capabilities and lifecycle semantics, see the
[versioned Rust SDK guide](../../../docs/sdks/rust.md).

## Features and development

Optional features are `redis` for pool coordination and `tracing` for OpenTelemetry
warmup spans. See the [Client Pool guide](../../../docs/guides/client-pool.md#rust)
and [observability guide](../../../docs/sdks/observability.md#rust-tracing) for setup.
The [interpreter section](../../../docs/sdks/rust.md#code-interpreter) covers the
separate unpublished `opensandbox-code-interpreter` crate.

Use [sdks/AGENTS.md](../../AGENTS.md) for generation, feature-matrix checks and
packaging requirements. Generated `src/api/` files must be regenerated through
`scripts/generate_api.py`, never edited by hand.

See [the runtime test guide](../../../docs/sdks/rust.md#testing-and-validation) for
commands and prerequisites, [recorded results](../../../docs/sdks/rust.md#recorded-live-results)
for validation scope, and [tests/rust](../../../tests/rust/README.md) for the test entrypoint.
