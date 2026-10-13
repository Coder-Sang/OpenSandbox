---
title: SDKs
description: Choose an OpenSandbox SDK and compare lifecycle, Client Pool, tracing, and diagnostics support.
---

# SDKs

OpenSandbox provides six sandbox SDKs for lifecycle management, command execution,
file operations, and resource metrics. This site follows the repository's default
branch; a feature merged here may require a newer package than the one you have
installed. Check the [release index](https://github.com/opensandbox-group/OpenSandbox/releases) when upgrading.

## Installation

| Language | Package | Install |
| --- | --- | --- |
| [Python](/sdks/python) | `opensandbox` | `pip install opensandbox` |
| [JavaScript/TypeScript](/sdks/javascript) | `@alibaba-group/opensandbox` | `npm install @alibaba-group/opensandbox` |
| [Kotlin/Java](/sdks/kotlin) | `com.alibaba.opensandbox:sandbox` | Gradle/Maven |
| [Go](/sdks/go) | `github.com/alibaba/OpenSandbox/sdks/sandbox/go` | `go get github.com/alibaba/OpenSandbox/sdks/sandbox/go` |
| [C#/.NET](/sdks/csharp) | `Alibaba.OpenSandbox` | `dotnet add package Alibaba.OpenSandbox` |
| [Rust](/sdks/rust) | `opensandbox` (unpublished) | Cargo path or Git dependency |

## Capability coverage

“Yes” means a public SDK entry point exists. Availability also depends on the
server runtime and the execd version installed in the sandbox.

| Capability | Python async/sync | JavaScript/TypeScript | Kotlin/Java | Go | C#/.NET | Rust |
| --- | --- | --- | --- | --- | --- | --- |
| Create, connect, renew, pause/resume, kill | Yes | Yes | Yes | Yes | Yes | Yes |
| Snapshots and template management | Yes | Yes | Yes | Yes | Yes | Yes |
| Resource requests, volumes, lifecycle hooks | Yes | Yes | Yes | Yes | Yes | Yes |
| Commands, files, resource metrics | Yes | Yes | Yes | Yes | Yes | Yes |
| Egress policy | Yes | Yes | Yes | Yes | Yes | Yes |
| Credential Vault | Yes | Yes | Yes | Yes | Yes | Yes |
| Isolated sessions | Yes | Yes | Yes | Yes | Yes | Yes |
| [Client Pool](/guides/client-pool), including Redis | Yes | Yes | Yes | Yes | No | Yes, optional `redis` |
| [Pool warmup tracing](/sdks/observability#pool-warmup-tracing) | Yes | Yes, fewer attributes | Yes | No | No | Yes, optional `tracing`, limited schema |
| [Remote diagnostic logs/events](/api/#diagnostics) | Yes | No | Yes | No | No | Yes |
| [Create-latency telemetry](/sdks/observability#creation-metrics) | Yes | Yes | Yes | Yes | Yes | Yes |

### Differences that affect application code

- **Rust:** the async Tokio/reqwest crate is installed from source, not crates.io.
  It supports command and isolated sessions, background commands, diagnostics,
  Credential Vault, and pools. The independent [Code Interpreter crate](/sdks/rust#code-interpreter)
  is also unpublished. Both use Cargo path/Git dependencies. Dropping a handle
  does not kill its remote sandbox. Public timeouts use `std::time::Duration`.

- **Pools:** Go exposes different warmup controls; the fields accepted by the default
  pool creator also differ across languages. See the [pool configuration matrix](/guides/client-pool#configuration).
- **Go command control:** interrupt, command status, and accumulated command logs
  are exposed by `ExecdClient`, rather than the high-level `Sandbox` wrapper.
  Neither `Sandbox.CreateSession` nor `ExecdClient.CreateSession` accepts an initial
  working directory. Set `Cwd` when running a command in the session instead.
- **Go connect/resume readiness:** pass `ReadyOptions` to request readiness checks.
  Without it, connecting resolves the endpoint without checking sandbox health.
- **Metrics streaming:** Go exposes `ExecdClient.WatchMetrics` and Rust exposes
  `sandbox.metrics().watch().await?`; other SDKs' stable metrics services provide
  point-in-time reads. The [CLI](/cli/) also has
  a metrics stream via `osb sandbox metrics --watch`.
- **Timeout units:** use Python `timedelta`, JVM `Duration` and Rust `std::time::Duration`; JavaScript and C#
  command timeouts use seconds. Go `RunCommandRequest.Timeout` uses **milliseconds**.

Template-backed sandboxes require an explicit TTL and inherit their workload
configuration from the published template. Their egress policy is managed through
the lifecycle API; they do not have a sandbox-side Credential Vault.

## Code Interpreter SDKs

Code Interpreter SDKs add persistent language contexts and rich code execution
results to a sandbox running an interpreter-enabled image. Rust provides the
standalone `opensandbox-code-interpreter` crate alongside `opensandbox`, both
unpublished. See [Rust Code Interpreter](/sdks/rust#code-interpreter) for path/Git
installation and the difference between kernel errors and transport failures.

## Feature guides

- [Client Pool](/guides/client-pool): keep a ready buffer, select an acquire policy,
  share state through Redis, and retire a pool namespace.
- [Observability](/sdks/observability): configure pool warmup traces and creation
  metrics, understand the default settings, and locate slow startup phases.

## Diagnostics

Python, Kotlin/Java, and Rust expose remote logs/events on both `Sandbox` and
`SandboxManager`. Use a manager when execd is not ready. Python methods are
`get_diagnostic_logs` / `get_diagnostic_events`; JVM methods are
`getDiagnosticLogs` / `getDiagnosticEvents`. Pass a sandbox ID to manager methods
and an explicit scope such as `container` for logs or `runtime` for events.

Rust uses `diagnostic_logs` / `diagnostic_events` and returns delivery descriptors,
not automatically downloaded content. See the [Rust guide](/sdks/rust#diagnostics).
JavaScript, Go, and C# can use the [CLI](/cli/#collect-diagnostics) or
[HTTP API](/api/#diagnostics). C# `SdkDiagnosticsOptions` controls local SDK
logging. See the API reference for supported scopes and inline/URL delivery.

## Lifecycle and cleanup

High-level image/snapshot creation defaults to a 10-minute TTL. Configure the TTL
explicitly for your workload. To disable expiration, use Python `timeout=None`,
JavaScript `timeoutSeconds: null`, Kotlin `timeout(null)`, or the Go/C#
`ManualCleanup` option, or Rust `CreateSandboxOptions.timeout = None`.
Template creation requires a TTL.

`close()`, `DisposeAsync()`, and context-manager exit release local client resources;
they do not kill the remote sandbox. Kill it in a `finally`/`defer` block, or use
Python's `destroy()` helper. Pool acquisitions are consumed once and are not returned
to the idle buffer.

## CLI

Use the [CLI](/cli/) to manage sandboxes, run commands, and work with files from a
terminal.

```bash
uv tool install opensandbox-cli
```

## MCP server

The [MCP server](/sdks/mcp) exposes sandbox operations to MCP-capable clients:

```bash
pip install opensandbox-mcp
```
