---
title: Rust Code Interpreter SDK
description: Install the standalone Rust Code Interpreter crate from source and use persistent contexts, streams, rich results, and explicit cleanup.
---

# Rust Code Interpreter SDK

`opensandbox-code-interpreter` is an independent Rust 2021 crate with MSRV 1.85.
It wraps an existing `opensandbox::Sandbox`, rather than creating a remote sandbox
itself. Use the [sandbox SDK](/sdks/rust) for lifecycle management and an
[interpreter-enabled image](/examples/code-interpreter) for code execution.
An ordinary Python image is not sufficient.

::: warning Both crates are unpublished
Neither `opensandbox` nor `opensandbox-code-interpreter` is published on crates.io.
Use source path or Git dependencies. Pin a reviewed revision and commit your
application's `Cargo.lock`.
:::

## Runtime requirements

The sandbox must start Jupyter Server with a kernel for the requested language,
not just install Python or Jupyter packages. Configure execd's `JUPYTER_HOST`
with the server's HTTP(S) URL and `JUPYTER_TOKEN` with the matching nonempty token.
For the default readiness probe, keep `JUPYTER_PORT` aligned with that server's
port. If sandbox creation overrides the image entrypoint, explicitly start
Jupyter there rather than relying on the image's `CMD`.

The official OpenSandbox 1.1.1 execd skips the stock `python3` kernelspec when
selecting a Python kernel. In the dedicated Rust acceptance test, a stock-only
image timed out during context creation. That runtime requires a matching spec
registered under another name, such as `python`. This SDK does not change execd's
kernel selection. A successful
readiness probe does not verify kernelspec availability or a complete execution
stream.

## Installation

Paths are relative to your application's manifest:

```toml
[dependencies]
opensandbox = { path = "../OpenSandbox/sdks/sandbox/rust" }
opensandbox-code-interpreter = { path = "../OpenSandbox/sdks/code-interpreter/rust" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Alternatively, use the same Git revision for both crates. Cargo discovers the
packages within the repository:

```toml
[dependencies]
opensandbox = { git = "https://github.com/opensandbox-group/OpenSandbox.git", rev = "<commit-containing-rust-sdk>" }
opensandbox-code-interpreter = { git = "https://github.com/opensandbox-group/OpenSandbox.git", rev = "<commit-containing-rust-sdk>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Attach and execute

Set `OPEN_SANDBOX_API_KEY` and `OPEN_SANDBOX_ID` for an existing interpreter sandbox.
This example takes responsibility for killing that sandbox, including on execution
failure. Do not use it against a sandbox that another application still needs.

```rust
use opensandbox::{ConnectionConfig, SandboxManager};
use opensandbox_code_interpreter::{CodeInterpreter, CodeInterpreterOptions, RunCodeOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ConnectionConfig::builder()
        .domain("localhost:8080")
        .api_key(std::env::var("OPEN_SANDBOX_API_KEY")?)
        .build()?;
    let manager = SandboxManager::new(config)?;
    let sandbox = manager.connect(&std::env::var("OPEN_SANDBOX_ID")?).await?;
    // Keep a handle for cleanup even when interpreter readiness fails.
    let cleanup_handle = sandbox.clone();
    let result = async {
        let interpreter = CodeInterpreter::create(sandbox, CodeInterpreterOptions::default()).await?;
        let context = interpreter.codes().create_context("python").await?;
        let execution = interpreter.codes().run("print(42)", RunCodeOptions {
            context: Some(context),
            ..Default::default()
        }).await?;
        for output in &execution.logs.stdout {
            print!("{}", output.text);
        }
        if execution.error.is_some() {
            return Err(opensandbox::Error::Protocol("kernel execution failed".into()));
        }
        Ok::<(), opensandbox::Error>(())
    }.await;
    let cleanup = cleanup_handle.kill().await;
    result?;
    cleanup?;
    Ok(())
}
```

`codes().run` returns an aggregated `Execution`. Kernel failures appear in
`Execution.error`, not a process exit code. Transport/protocol failures return
`Err`. Rich MIME results are retained in `ExecutionResult::raw`; inspect the MIME
keys appropriate for your renderer rather than treating every result as plain text.

## Persistent contexts and streams

- `create_context(language)` creates a persistent kernel context. Use
  `create_context_with_options(language, CreateContextOptions { cwd: Some(path) })`
  to set its initial working directory. The runtime must be able to create or
  access that directory in the sandbox.
- `get_context(id)` and `list_contexts(Some(language))` inspect contexts. Use `None`
  to list without a language filter.
- `delete_context(id)` deletes one context; `delete_contexts(language)` deletes all
  contexts of that language in this sandbox. Use bulk deletion only when you own them.
- `run(code, options)` aggregates output; `run_stream(code, options)` returns an
  `ExecutionStream` of typed `ExecutionEvent` results.
- `interrupt(context_id)` explicitly interrupts the context's remote execution.

Specify either `RunCodeOptions.context` or `RunCodeOptions.language`, not both.
Language defaults to Python when neither is provided. Available languages and rich
output formats depend on the sandbox image and execd runtime.
Dropping a stream does not interrupt the remote kernel. Dropping a context handle
or interpreter facade does not delete contexts or kill the sandbox.

## Execution stream expectations

The SDK uses the existing execd SSE contract: `stdout`, `stderr`, and `result`
events carry output, `error` carries a kernel failure, and `execution_complete`
marks successful completion. Output must arrive before the terminal event.
A kernel failure does not require a subsequent successful completion event.
`codes().run` stops aggregation on `execution_complete`. An `error` populates
`Execution.error`, but aggregation continues until the response body ends, so
failure streams must close cleanly. Transport failures while draining still
return `Err`. A stream ending without either terminal event is a protocol
failure rather than partial output accepted as success.

An `execution_count` event is optional and is not a prerequisite for retaining a
result. Rust preserves MIME bundles delivered by the runtime in
`ExecutionResult::raw`. The SDK cannot recover output omitted by execd or output
sent after a successful completion marker, and it does not suppress transport
errors or retry executions automatically.

::: warning Runtime compatibility is separate from SDK validation
This Rust SDK change does not include execd or ingress fixes. A runtime that
omits rich outputs, finalizes before output is delivered, or truncates a stream
can fail the full interpreter acceptance scenario even if readiness and context
creation succeed. Validate persistent state, rich results, kernel errors,
interruption, and immediate context reuse against the deployed runtime.
SDK unit tests passing does not establish live-runtime acceptance.

With the official 1.1.1 release BOM images, the direct compatibility-kernel
scenario passed context creation, stdout and persistent-state checks, then failed
its strict HTML-output assertion: the execution returned an empty result list
with `complete: true`. Later JSON, kernel-error, interruption and immediate-reuse
checks were not reached. The full interpreter workflow is therefore not accepted
against that release, and this SDK change does not patch its runtime.

A separate run correcting only the host-run server's proxy endpoint resolution
also failed. The direct compatibility scenario encountered an event JSON missing
`type` while initializing persistent state; the proxy scenario failed the state
check with an empty stdout list and `complete: true`. The missing-field failure's
precise cause was not established from that run. Stock-only context creation
timed out in both modes. These observations are retained as failures, not retries
that establish interpreter acceptance.
:::

## Readiness and cleanup

`CodeInterpreter::create` checks execd and an actual Jupyter TCP connection inside
the sandbox. The runtime probe requires Bash and checks `JUPYTER_PORT` (default
`44771`). The default total readiness budget is 30 seconds, with 200-millisecond
polling. `CodeInterpreterOptions.ready_timeout` and `polling_interval` use native
`std::time::Duration`. Set `skip_health_check` only when the application performs
its own readiness validation.

A readiness timeout does not kill the sandbox. Retain a cloned sandbox handle for
cleanup if attachment fails. Delete contexts explicitly when retaining the sandbox,
and call `interpreter.sandbox().kill().await?` when the sandbox is no longer needed.
Keep a server-side sandbox TTL as a fallback for crashes or failed cleanup.

The facade also exposes `commands()`, `files()`, and `metrics()`. These delegate to
the sandbox services and share their runtime and cleanup limitations.
