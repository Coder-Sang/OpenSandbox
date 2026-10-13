# OpenSandbox Code Interpreter Rust SDK

Independent, unpublished Rust 2021 crate, MSRV 1.85. Wraps an existing sandbox
running an interpreter-enabled image. Install both crates from source; paths
below are relative to your application's manifest:

```toml
[dependencies]
opensandbox = { path = "../OpenSandbox/sdks/sandbox/rust" }
opensandbox-code-interpreter = { path = "../OpenSandbox/sdks/code-interpreter/rust" }
```

From this package directory, run the packaged attach example with an existing
interpreter sandbox:

```bash
export OPEN_SANDBOX_DOMAIN=localhost:8080
export OPEN_SANDBOX_API_KEY=your-api-key
export SANDBOX_ID=your-interpreter-sandbox-id
cargo run --locked --example attach
```

The [example](examples/attach.rs) prints execution output and checks kernel
errors. It leaves the existing sandbox alive. Your application owns context
cleanup and sandbox destruction; dropping a facade or stream does not delete
contexts, kill the sandbox or interrupt the kernel.

See [the Rust SDK interpreter section](../../../docs/sdks/rust.md#code-interpreter)
for context APIs, streaming, Git installation, runtime readiness and cleanup;
[recorded interpreter results](../../../docs/sdks/rust.md#interpreter-results)
describe the tested runtime's limitations.
For generation, quality checks and registry packaging restrictions, follow
[the SDK contributor instructions](../../AGENTS.md).
