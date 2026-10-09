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

## Features and cleanup

The standalone `opensandbox-code-interpreter` crate is also unpublished. See
[Rust Code Interpreter](../../../docs/sdks/code-interpreter-rust.md) for path/Git
installation and interpreter runtime requirements.

Enable the optional `redis` Cargo feature for `RedisPoolStateStore`, or `tracing`
for OpenTelemetry warmup spans. Tracing additionally requires connection
`.enable_tracing(true)` and an application-owned provider/exporter, and defaults
to off. Best-effort creation metrics default to on. Disable them with
`.disable_metrics(true)` or `OPENSANDBOX_DISABLE_METRICS=1`.

Pool acquisitions are consumed once with no return-to-pool release API. Kill
acquired sandboxes explicitly and stop pools before Tokio exits. See the
[Client Pool guide](../../../docs/guides/client-pool.md#rust). Diagnostics return
inline/expiring-URL descriptors and never fetch external URLs automatically.
Public timeout configuration uses `std::time::Duration`.

## Development

```bash
python3 -m pip install -r scripts/requirements.txt
python3 scripts/generate_api.py --check
python3 -m unittest discover -s scripts -p 'test_*.py'
cargo +1.85.1 fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo doc --locked --no-deps
cargo package --locked
```

`src/api/` is generated from the repository's lifecycle, execd, egress, and diagnostic OpenAPI specifications.
Regenerate it with `python3 scripts/generate_api.py`, not by editing generated
files. Public configuration and service facades are handwritten.

Real-server tests live in [tests/rust](../../../tests/rust/). From the repository
root, run `bash scripts/rust-e2e.sh` for an isolated Docker-backed test server, or
set `OPENSANDBOX_TEST_DOMAIN` to test an existing server. The script does not
change `~/.sandbox.toml` or delete unrelated Docker resources.
