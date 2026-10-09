# OpenSandbox Rust SDK

Async Rust client for the OpenSandbox lifecycle, execd, and network policy APIs.
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

`src/api/` is generated from the repository's three OpenAPI specifications.
Regenerate it with `python3 scripts/generate_api.py`, not by editing generated
files. Public configuration and service facades are handwritten.

Real-server tests live in [tests/rust](../../../tests/rust/). From the repository
root, run `bash scripts/rust-e2e.sh` for an isolated Docker-backed test server, or
set `OPENSANDBOX_TEST_DOMAIN` to test an existing server. The script does not
change `~/.sandbox.toml` or delete unrelated Docker resources.
