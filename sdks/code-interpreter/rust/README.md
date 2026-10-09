# OpenSandbox Code Interpreter Rust SDK

Independent, unpublished Rust 2021 crate, version 1.1.0, MSRV 1.85.
Use a local path dependency on this package and the sandbox SDK.

```toml
[dependencies]
opensandbox = { path = "sdks/sandbox/rust", version = "1.1.0" }
opensandbox-code-interpreter = { path = "sdks/code-interpreter/rust", version = "1.1.0" }
```

Wrap an existing sandbox running a code-interpreter image:

```rust,no_run
use opensandbox::Sandbox;
use opensandbox_code_interpreter::{CodeInterpreter, CodeInterpreterOptions, RunCodeOptions};

async fn execute(sandbox: Sandbox) -> opensandbox::Result<()> {
    let interpreter = CodeInterpreter::create(sandbox, CodeInterpreterOptions::default()).await?;
    let context = interpreter.codes().create_context("python").await?;
    let result = interpreter.codes().run("print(42)", RunCodeOptions {
        context: Some(context.clone()), ..Default::default()
    }).await?;
    for output in result.logs.stdout { print!("{}", output.text); }
    interpreter.codes().delete_context(context.id.as_deref().expect("context ID")).await?;
    Ok(())
}
```

`codes()` supports aggregated and streaming execution, context create/get/list/delete,
language-filtered bulk deletion, and interrupt. `create_context_with_options` accepts
`CreateContextOptions { cwd: Some("/workspace".into()) }` for an initial working directory.
Rich MIME results remain in
`ExecutionResult::raw`. Specify either a context or a language, not both.

Creation checks execd and an actual Jupyter TCP connection inside the sandbox.
The default total readiness budget is 30 seconds, polling every 200 milliseconds.
Both are native `Duration` values. `skip_health_check` explicitly opts out.
Timeouts and dropping the facade do not kill the sandbox. Use `sandbox().kill()`
for explicit remote cleanup. Dropping an execution stream does not interrupt its
remote kernel, use `codes().interrupt(context_id)` when needed.

See [Rust SDK documentation](../../../docs/sdks/rust.md) for sandbox setup.
Run `cargo test` and `cargo clippy --all-targets -- -D warnings` in this directory.
The `attach` example uses an existing code-interpreter sandbox ID.
