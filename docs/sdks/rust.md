---
title: Rust SDK
description: Async Rust sandbox SDK with isolated sessions, Credential Vault, diagnostics, pools, and optional Redis and tracing.
---

# OpenSandbox Rust SDK

The `opensandbox` crate is an async Rust SDK built on Tokio and reqwest. It lives
in `sdks/sandbox/rust` as an independent crate, outside the JavaScript workspace.
It uses Rust 2021 and requires Rust 1.85 or later.

The branch is based on the official OpenSandbox `release-1.1.1` tag. Generated
`api` models include that release's identity-scoped filesystem operations,
isolated-session overlays and template environment fields. These wire models do
not imply equivalent handwritten service coverage: the isolated-session facade
still uses its legacy workspace options, and the template facade does not yet
expose template environment variables. Crate versions remain `1.1.0`.

::: warning Source installation only
The first version is not published on crates.io. Use a local path or a Git
revision, not `cargo add opensandbox` from the registry. Pin a reviewed Git
commit for reproducible builds.
:::

## Installation

For a local checkout, add this dependency to your application's `Cargo.toml`.
Adjust the path relative to that manifest:

```toml
[dependencies]
opensandbox = { path = "../OpenSandbox/sdks/sandbox/rust" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Alternatively, use the repository as a Git dependency. Replace the placeholder
with a commit that contains the Rust crate:

```toml
[dependencies]
opensandbox = { git = "https://github.com/opensandbox-group/OpenSandbox.git", rev = "<commit-containing-rust-sdk>" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Cargo locates the `opensandbox` package within the repository. Commit your
application's `Cargo.lock` to keep its dependency resolution reproducible.

## Quick Start

Start an OpenSandbox server following the [quick start](/getting-started/), then
run this example in your application. Set `OPEN_SANDBOX_API_KEY` to match your
server configuration:

```rust
use opensandbox::{
    ConnectionConfig, CreateSandboxOptions, RunCommandOptions, SandboxManager,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ConnectionConfig::builder()
        .domain("localhost:8080")
        .api_key(std::env::var("OPEN_SANDBOX_API_KEY")?)
        .build()?;
    let manager = SandboxManager::new(config)?;
    let sandbox = manager
        .create(CreateSandboxOptions::image("python:3.12"))
        .await?;

    let command_result = sandbox
        .commands()
        .run("echo hello", RunCommandOptions::default())
        .await;
    // Attempt cleanup even if the command failed.
    let cleanup_result = sandbox.kill().await;
    let execution = command_result?;
    cleanup_result?;
    if execution.error.is_some() || execution.exit_code.is_some_and(|code| code != 0) {
        return Err(std::io::Error::other("sandbox command failed").into());
    }
    Ok(())
}
```

Transport and protocol failures return `Err`. A command failure is reported in
`Execution.error` or a nonzero `Execution.exit_code`, so inspect the returned
execution rather than treating every `Ok` as command success.
`RunCommandOptions.timeout` uses `std::time::Duration`, not an integer number
of seconds. Cancelling or dropping a command stream does not automatically
interrupt the remote command.

The standard connection environment variables are `OPEN_SANDBOX_DOMAIN` and
`OPEN_SANDBOX_API_KEY`. The example sets the domain explicitly.

## Files

For a live `sandbox`, use text helpers for UTF-8 content:

```rust
sandbox.files().write_text("/tmp/hello.txt", "hello").await?;
let text = sandbox.files().read_text("/tmp/hello.txt").await?;
```

For binary content, use `upload(path, Vec<u8>)` and `download(path)`, which
returns `Vec<u8>`. As with commands, ensure sandbox cleanup is attempted if a
file operation fails.

## Capability coverage

The sandbox crate covers lifecycle and sandbox services. Code execution is provided
by the separate [Rust Code Interpreter SDK](/sdks/code-interpreter-rust). Server
runtime, egress sidecar, and execd support determine operation availability.

| Capability | Rust SDK |
| --- | --- |
| Sandbox lifecycle and metadata | Yes |
| Snapshots and templates | Yes |
| Command execution, command sessions, and background commands | Yes |
| File operations | Yes |
| Resource metrics | Yes |
| Network policy | Yes |
| Code Interpreter SDK | Yes, separate `opensandbox-code-interpreter` crate |
| Client Pool, including Redis | Yes, Redis requires the `redis` feature |
| Isolated sessions | Yes, subject to execd capabilities |
| Credential Vault | Yes, sandbox-local egress sidecar only |
| SDK telemetry and pool tracing | Yes, tracing requires the `tracing` feature |
| Remote diagnostic logs/events | Yes, delivery descriptors without automatic downloads |

Command sessions and isolated sessions are different execd services. Check
`sandbox.isolated().capabilities().await?` before choosing an isolation
profile. Resource metrics support point-in-time reads and `sandbox.metrics().watch().await?`.

## Lifecycle and cleanup

Sandbox operations are asynchronous and must run within a Tokio runtime.
`CreateSandboxOptions.timeout` uses `Option<std::time::Duration>` and defaults
to 600 seconds for image/snapshot creation. Set it to `None` for manual cleanup
without expiration. Template creation requires an explicit TTL.
Configure the server connection and credentials for your deployment using the
[server configuration guide](/getting-started/configuration).

Explicitly call `sandbox.kill().await?` when the remote sandbox is no longer
needed. Dropping a Rust handle does **not** kill the sandbox. Arrange cleanup
on error paths as well as the successful path, and configure a server-side TTL
as a fallback for process crashes or failed cleanup requests.

If creation succeeds remotely but subsequent setup or readiness checks fail,
`Error::SandboxSetup` and `Error::ReadyTimeout` preserve the sandbox ID. Use that ID to explicitly clean
up the remote sandbox through the manager or the [CLI](/cli/). A failed create
call does not necessarily mean that no remote resource exists.

## Isolated sessions

`sandbox.isolated()` exposes capabilities, list/create/attach/get/delete,
foreground and streamed execution, background runs, status, logs, and session-scoped
files. Profiles depend on execd and host isolation support, not just SDK availability.
Public timeouts use `std::time::Duration`; isolated idle timeouts must be whole seconds.

Delete long-lived sessions explicitly with `session.delete().await?`. Dropping a
session handle or execution stream does not delete the session or interrupt its
remote run. The `run_once` helper returns `RunOnceResult` with separate `execution`
and `cleanup` results and a `session_id` for retrying failed cleanup. It attempts
cleanup on cancellation while Tokio is alive; runtime shutdown and process crashes
can still prevent cleanup. Keep server-side idle GC as a fallback.

## Credential Vault

`sandbox.egress().await?` exposes vault `create`, `get`, `patch`, `delete`, credential
metadata `list_credentials` / `get_credential`, and binding metadata
`list_bindings` / `get_binding`. Request types live in `opensandbox::api::egress`.
Secret values are write-only. Use revisions for guarded mutations and avoid logging
request values. See the [Credential Vault guide](/guides/credential-vault).
Template-backed sandboxes support lifecycle-managed network policy but do not
expose a sandbox-local Credential Vault.

## Diagnostics

`manager.diagnostic_logs(sandbox_id, "container").await?` and
`manager.diagnostic_events(sandbox_id, "runtime").await?` also work when execd is
not ready. The sandbox provides `diagnostic_logs(scope)` and
`diagnostic_events(scope)` conveniences. Runtime support determines valid scopes.

Results are `DiagnosticContent` delivery descriptors. Match `DiagnosticPayload::Inline`
for text or `DiagnosticPayload::Url` for an expiring HTTP(S) download URL. The SDK
never fetches these URLs or forwards server credentials to them. Apply your own
trusted-host policy before downloading, and check `truncated` and `warnings`.
Diagnostic text is not a stable structured event schema. See [diagnostics](/api/#diagnostics).

## Client Pool and optional features

The [Client Pool guide](/guides/client-pool#rust) documents `PoolConfig`,
`SandboxPool`, acquisition policies, and Redis coordination. Acquisitions consume
a sandbox once. There is no return-to-pool `release()` API. Explicitly kill acquired
sandboxes and stop the pool before shutting down Tokio.

Optional Cargo features are disabled by default:

```toml
opensandbox = { path = "../OpenSandbox/sdks/sandbox/rust", features = ["redis", "tracing"] }
```

`redis` enables `RedisPoolStateStore`. Its namespace layout is Rust-specific, not
interoperable with other language pool stores. `tracing` enables OpenTelemetry
warmup instrumentation and additionally requires `.enable_tracing(true)` and an
application-owned provider/exporter. Tracing is off by default.

Creation-latency telemetry is best-effort and on by default, independently of Cargo
features and tracing. Disable it with `.disable_metrics(true)` on the connection
builder or `OPENSANDBOX_DISABLE_METRICS=1`. Reporting failures do not replace creation
errors. See [observability](/sdks/observability#rust-tracing) for the Rust trace scope
and [creation metrics](/sdks/observability#creation-metrics) for telemetry controls.

## Code Interpreter

The independent, unpublished `opensandbox-code-interpreter` crate wraps an existing
`Sandbox` and provides persistent language contexts, execution streams, rich results,
and interrupts. It requires an image with the interpreter runtime, not an arbitrary
Python image. See [Rust Code Interpreter](/sdks/code-interpreter-rust) for source
installation, readiness checks, and cleanup.

## Development

See [the SDK contributor instructions](https://github.com/opensandbox-group/OpenSandbox/blob/main/sdks/AGENTS.md)
for generator ownership and local checks. The [SDK overview](/sdks/) compares
Rust with the other language SDKs.
