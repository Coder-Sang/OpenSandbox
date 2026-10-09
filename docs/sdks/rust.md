---
title: Rust SDK
description: Async Rust client for OpenSandbox lifecycle, command execution, files, metrics, and network policy.
---

# OpenSandbox Rust SDK

The `opensandbox` crate is an async Rust SDK built on Tokio and reqwest. It lives
in `sdks/sandbox/rust` as an independent crate, outside the JavaScript workspace.
It uses Rust 2021 and requires Rust 1.85 or later.

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

The Rust SDK covers the sandbox API, not the Code Interpreter SDK. Server runtime
and execd support still determine whether a particular operation is available.

| Capability | Rust SDK |
| --- | --- |
| Sandbox lifecycle and metadata | Yes |
| Snapshots and templates | Yes |
| Command execution, command sessions, and background commands | Yes |
| File operations | Yes |
| Resource metrics | Yes |
| Network policy | Yes |
| Code Interpreter SDK | No |
| Client Pool, including Redis | No |
| Isolated sessions | No |
| Credential Vault | No |
| SDK telemetry and pool tracing | No |
| Remote diagnostic logs/events | No |

Command sessions are execd command sessions. They are not the isolated-session
feature exposed by other language SDKs. For unsupported operations, use another
SDK or the [HTTP API](/api/), as appropriate.

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

## Development

See [the SDK contributor instructions](https://github.com/opensandbox-group/OpenSandbox/blob/main/sdks/AGENTS.md)
for generator ownership and local checks. The [SDK overview](/sdks/) compares
Rust with the other language SDKs.
