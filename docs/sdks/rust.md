---
title: Rust SDK
description: Install and use the Rust sandbox and interpreter SDKs, configure optional features, and run tests with recorded validation results.
---

# OpenSandbox Rust SDK

The `opensandbox` crate is an async Rust SDK built on Tokio and reqwest. It lives
in `sdks/sandbox/rust` as an independent crate, outside the JavaScript workspace.
It uses Rust 2021 and requires Rust 1.85 or later.

Wire models and internal typed clients are generated from the repository's
OpenAPI specifications. Ordinary requests use those clients; handwritten facades
provide the stable public API, duration conversion, cleanup policy and SSE parsing.
Runtime support determines whether isolated sessions, templates and interpreter
operations are available. SDK coverage alone does not establish runtime acceptance.

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
For an application targeting Rust 1.85, set `rust-version = "1.85"` in its
`[package]` table and configure Cargo's MSRV-aware resolution in `.cargo/config.toml`:

```toml
[resolver]
incompatible-rust-versions = "fallback"
```

Generate the lockfile with that configuration and check it on the target compiler.
The SDK's MSRV checks use its committed lockfile; a fresh application resolving
unconstrained transitive versions can otherwise select dependencies requiring a
newer compiler. The application's lockfile controls path and Git dependencies too.


## Quick Start

Start an OpenSandbox server following the [quick start](/getting-started/), then
run this example in your application. Set `OPEN_SANDBOX_API_KEY` to match your
server configuration:

```rust
use opensandbox::{
    ConnectionConfig, CreateSandboxOptions, Error, RunCommandOptions, SandboxManager,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ConnectionConfig::builder()
        .domain("localhost:8080")
        .api_key(std::env::var("OPEN_SANDBOX_API_KEY")?)
        .build()?;
    let manager = SandboxManager::new(config)?;
    let sandbox = match manager.create(CreateSandboxOptions::image("python:3.12")).await {
        Ok(sandbox) => sandbox,
        Err(error) => {
            if let Error::ReadyTimeout { sandbox_id, .. } | Error::SandboxSetup { sandbox_id, .. } = &error {
                manager.kill(sandbox_id).await?;
            }
            return Err(error.into());
        }
    };

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

For native executable arguments, use `run_argv` or `run_argv_stream` with the
same `RunCommandOptions` and execution types. Arguments are sent literally,
including empty arguments, spaces, shell metacharacters and environment-variable
syntax; they are not joined into shell text:

```rust
let execution = sandbox.commands().run_argv(
    &["python3", "-c", "import sys; print(sys.argv[1])", "$HOME"],
    RunCommandOptions::default(),
).await?;
```

Provide at least one argument and a nonempty executable name. No argument may
contain NUL. Background execution, timeout, cwd, environment, UID/GID and cleanup
semantics match `run`. Existing `run` and command-session methods continue to
accept shell text; command sessions do not support native argv.

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

### Filesystem execution identity

Create a file handle bound to an explicit Linux execution identity. The target
directory must exist and permit that identity to write; this example assumes a
world-writable `/tmp` and a new file:

```rust
let files = sandbox.files().with_identity(1000, 1000)?;
files.write_text("/tmp/identity-hello.txt", "hello").await?;
let text = files.read_text("/tmp/identity-hello.txt").await?;
```

`with_identity(uid: u32, gid: u32)` returns a new `Files` handle covering all
11 file and directory operations through `/v1/filesystem/{uid}/{gid}`. IDs must
be between 0 and 4294967294; the reserved `u32::MAX` value is rejected before
HTTP. The original handle keeps its scope, and calling `with_identity` again on
an identity handle replaces that identity. Identity selection is separate from
upload or permission metadata such as owner, group and mode. Metadata `owner`
and `group` are account names, so those accounts must exist even when the execution
identity uses numeric IDs.

The server must support Linux identity switching. API errors, including an
unknown route on an older server, are returned without retrying an ordinary
filesystem route. Keep API credentials in a trusted backend that chooses the
identity. Isolated-session file handles reject `with_identity` because identity
routes do not preserve isolated-session confinement.

## Capability coverage

The sandbox crate covers lifecycle and sandbox services. Code execution is provided
by the separate [Rust Code Interpreter SDK](#code-interpreter). Server
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
to 600 seconds with the image, snapshot and template constructors. Set it to
`None` for manual cleanup without expiration where supported. Template-backed
creation requires a non-`None`, positive TTL; the constructor supplies that default.
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

When creating an image-backed sandbox for isolated sessions, set
`options.extensions.insert("bootstrap.execd.isolation".into(), "enable".into())`
before calling `manager.create(options)`. This requests the container-specific
permissions required by the [isolation runtime](/guides/isolation-sessions).
The SDK forwards this existing extension; it does not enable isolation implicitly.

Delete long-lived sessions explicitly with `session.delete().await?`. Dropping a
session handle or execution stream does not delete the session or interrupt its
remote run. The `run_once` helper returns `RunOnceResult` with separate `execution`
and `cleanup` results and a `session_id` for retrying failed cleanup. It attempts
cleanup on cancellation while Tokio is alive; runtime shutdown and process crashes
can still prevent cleanup. Keep server-side idle GC as a fallback.

### Multiple overlays and overlays-only sessions

Existing `CreateIsolatedSessionOptions`, `create` and `run_once` remain available.
Use `CreateIsolatedSessionAdvancedOptions` with `create_advanced` or
`run_once_advanced` when `workspace` is optional or multiple overlays are needed:

```rust
use opensandbox::{CreateIsolatedSessionAdvancedOptions, IsolatedOverlaySpec, OverlayMode};

let session = sandbox.isolated().create_advanced(CreateIsolatedSessionAdvancedOptions {
    overlays: vec![IsolatedOverlaySpec {
        path: "/workspace".into(),
        mode: Some(OverlayMode::Overlay),
        persist: Some(true),
        ..Default::default()
    }],
    ..Default::default()
}).await?;
// Run work, then explicitly delete even if the work fails.
session.delete().await?;
```

Provide at least one workspace or overlay. Paths must be absolute, unique and
clean Linux paths. A workspace, when supplied, precedes the overlays, and the
combined limit is 16 mounts. `persist` must be omitted for `rw` and `ro` modes.
The SDK cannot determine whether a path is accessible inside the sandbox.
An ephemeral overlay (`persist=false`) discards writes on deletion, does not
expose those writes through the files API, and cannot provide background-log
storage as the first mount. These are runtime semantics, not SDK workarounds.
Both one-shot methods retain the same execution/cleanup and cancellation rules.

## Template environment variables

Keep `manager.create_template(options)` for the existing interface, which omits
`env`. Use `manager.create_template_with_env(options, env)` to supply a
`BTreeMap<String, String>`, including an explicit empty map. Keys must match
`[A-Za-z_][A-Za-z0-9_]*`. Values are literal, not shell expressions. Template
variables override the source image's inherited environment and are baked into
the golden image, so do not use them as an ephemeral secret-injection mechanism.

The environment-aware create/get/list methods return `TemplateInfoWithEnv`
(or `ListTemplatesWithEnvResponse`) with the existing template information and
an optional `env` map. Use `get_template_with_env` and `list_templates_with_env`
when the server's environment echo must be retained. Existing methods retain
`TemplateInfo` and `ListTemplatesResponse` for source compatibility. Debug output
of the environment-aware wrapper redacts values. Full create/build/instantiate
acceptance requires a dedicated fast-sandbox runtime, artifact storage and KVM.

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
`Sandbox`. It provides persistent language contexts, streaming execution, rich
results and interruption. Add it alongside `opensandbox` using the same source
checkout or Git revision; the [installation and MSRV guidance](#installation)
applies to both:

```toml
opensandbox-code-interpreter = { path = "../OpenSandbox/sdks/code-interpreter/rust" }
```

### Runtime and readiness

Use an [interpreter-enabled image](/examples/code-interpreter) with Jupyter Server
running and a kernel for the requested language. Configure execd's `JUPYTER_HOST`
with its HTTP(S) URL and `JUPYTER_TOKEN` with the matching nonempty token. If the
sandbox entrypoint overrides the image's startup command, explicitly start Jupyter.
Installing Python or Jupyter alone is insufficient.

`CodeInterpreter::create` checks execd and a Jupyter TCP connection using Bash and
`JUPYTER_PORT` (default `44771`). The default readiness budget is 30 seconds with
200ms polling. `CodeInterpreterOptions.ready_timeout` and `polling_interval` use
`Duration`; `skip_health_check` opts out. This probe does not check authentication,
kernel availability or execution correctness. A timeout does not kill the sandbox.

Interpreter acceptance also depends on the execd build. The unmodified runtime
at `c8eadfd6` has known failures in stock `python3` selection, rich display
forwarding and execution completion. SDK readiness does not establish that
these protocol paths work. The Rust SDK preserves the public contract and
reports malformed or incomplete streams; it does not retry code execution or
fabricate missing output. See [interpreter results](#interpreter-results).

### Contexts and execution

For an existing `sandbox`, retain its handle for cleanup if interpreter attachment
fails. This example creates and deletes a context, leaving the sandbox alive:

```rust
use opensandbox_code_interpreter::{CodeInterpreter, CodeInterpreterOptions, RunCodeOptions};

let interpreter = CodeInterpreter::create(sandbox.clone(), CodeInterpreterOptions::default()).await?;
let codes = interpreter.codes();
let context = codes.create_context("python").await?;
let id = context.id.clone().ok_or_else(|| opensandbox::Error::Protocol("missing context ID".into()))?;
let result = codes.run("print(42)", RunCodeOptions {
    context: Some(context),
    ..Default::default()
}).await;
let cleanup = codes.delete_context(&id).await;
let execution = result?;
cleanup?;
if execution.error.is_some() {
    return Err(opensandbox::Error::Protocol("kernel execution failed".into()).into());
}
```

`create_context_with_options` accepts an initial `cwd`; that directory must be
accessible to the runtime. Use `get_context`, `list_contexts(Some(language))`,
`list_contexts(None)`, and `delete_context` for context management.
`delete_contexts(language)` deletes all contexts of that language; use it only
when you own them. Specify either `RunCodeOptions.context` or `language`, not both;
the default language is Python.

`run` returns an aggregated `Execution`; `run_stream` returns typed events.
Kernel failures appear in `Execution.error`, rather than a process exit code;
transport/protocol failures return `Err`. Rich MIME bundles remain in
`ExecutionResult::raw`. Aggregation stops at `execution_complete`; on an `error`,
it records the failure and drains until completion or EOF. Transport failures
while draining return `Err`, as does EOF without completion or a kernel error.
The SDK cannot recover omitted output or output sent after completion, and does
not automatically retry executions.

Use `interrupt(context_id)` to interrupt a remote execution. Dropping a stream,
context or interpreter does not interrupt execution, delete contexts or destroy
the sandbox. Delete contexts when retaining the sandbox; otherwise arrange
`sandbox.kill()` as described in [lifecycle and cleanup](#lifecycle-and-cleanup).
The facade's `commands`, `files` and `metrics` delegate to the sandbox services.

::: warning Interpreter runtime acceptance remains incomplete
This Rust SDK change does not modify execd or the lifecycle server. Earlier
passing interpreter matrices used an experimental modified execd; those results
are not acceptance of the unmodified runtime. Missing terminal events and the
original intermittent protocol failures remain unresolved. See
[the recorded live results](#interpreter-results).
:::

## Development

Follow [the SDK contributor instructions](https://github.com/opensandbox-group/OpenSandbox/blob/main/sdks/AGENTS.md)
for generated-client ownership, pinned generation tools, the MSRV/stable feature
matrix and packaging checks. Both crates are independent of the JavaScript
workspace. Interpreter registry package verification is blocked until the core
`opensandbox` dependency is published. See the [SDK overview](/sdks/) for language
comparisons.

## Testing and validation

All sandbox live tests are ignored by ordinary `cargo test`. Redis integration
tests require opt-in flags supplied by their runner. From `tests/rust`,
`cargo test --locked` checks local fixtures without allocating remote sandboxes.
The [test crate](https://github.com/opensandbox-group/OpenSandbox/tree/main/tests/rust)
and runner scripts are the executable coverage definitions.

The sandbox runner requires Docker, uv and Python 3. It starts a private server,
uses a fresh scratch directory and deletes only its own labelled resources.
Both direct and server-proxy modes run, including with an existing server.

### Live identity, argv and Redis tests

From the repository root, run the owned Docker scenarios in both direct and
server-proxy modes:

```bash
OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY=true \
  RUSTUP_TOOLCHAIN=1.85.1 bash scripts/rust-e2e.sh
```

This focused suite builds execd from the checked-out source and uses a Python 3
workload image. It checks all 11 identity file routes, binary/range/line reads,
actual kernel UID/GID and mode, denial of another identity's access and root
ownership changes, and absence of inherited root groups. Metadata `owner` and
`group` remain account names; numeric fixture ownership is prepared independently
with `os.chown`, without adding accounts. Native argv checks include empty,
Unicode and shell-metacharacter arguments, cwd/environment/UID/GID options,
streaming, background status/logs, nonzero exits and interruption. Existing root
file and shell-command handles are also exercised.

The dedicated Redis runner requires Docker, OpenSSL and Rust 1.85.1 and stable
installed:

```bash
bash scripts/rust-redis-e2e.sh
```

The Redis runner executes Rust 1.85.1 and stable by default; override with
`OPENSANDBOX_RUST_TOOLCHAINS`. It creates loopback-only plaintext and
password-protected TLS-only Redis containers, private short-lived certificates,
and a server certificate valid only for `localhost`. Four pool integration tests
run against each transport. Separate child processes test trusted certificates,
an unrelated CA, an invalid hostname and incorrect authentication, checking the
specific rejection cause. It never uses a shared Redis, flushes unrelated keys
or changes the system trust store. Artifacts remain in the printed private
directory after the invocation's containers are removed.

The workflows configure dedicated UID/GID and argv jobs on Rust 1.85.1 and
stable, each covering direct and server-proxy modes. Both matrix jobs feed the
Real E2E CI gate. SDK CI configures the Redis runner on each toolchain, including
TLS pool operations and certificate/authentication validation. Test and service
logs are uploaded on success or failure for five days; Redis certificates,
private keys and fixture configuration are excluded. Local scripts print their
artifact directories. This describes the configured jobs, not a verified remote
CI run.

### Full runtime tests

From the repository root, `bash scripts/rust-e2e.sh` builds execd, egress and a
Python/Jupyter fixture, then runs the basic, expanded, overlay and identity/argv
scenarios in both endpoint modes. The tests cover command and file behavior,
metrics, egress, isolation, Credential Vault and pool ownership. Interpreter
scenarios run when `RUN_CODE_INTERPRETER_E2E=true`; the default is false, matching
the other SDK runners and the workflow's `run_code_interpreter_e2e` input.
This selects scenarios without relaxing their assertions. The focused
`OPENSANDBOX_RUST_INTERPRETER_ONLY=true` command below explicitly selects the
interpreter regardless of the full-suite switch.
This is test coverage, not a record that every scenario passed.
The runner's host-server configuration uses `proxy.resolve_internal=false` so
proxy requests use Docker's published ports.

The full runner first checks isolation prerequisites in both endpoint modes,
using the same `bootstrap.execd.isolation=enable` extension as the other SDKs.
For Docker this lets the server grant the test container `CAP_SYS_ADMIN`,
unconfined AppArmor/seccomp and an isolation upper tmpfs. It does not change
the host's global security configuration. The preflight checks the trusted
workload gate and namespace probes, then creates a real overlay
session and verifies execution, scoped file access and cleanup. An unmet
prerequisite fails that mode before the main scenarios; it is not a passing skip.
CI retains `prerequisites-*.txt` alongside the other E2E logs.
The capability field `commit_supported` describes the unimplemented Phase 2
commit API; a false value is not an overlay mount failure. Actual session
execution and file checks establish the overlay prerequisites.

For a dedicated existing server, supply `OPENSANDBOX_TEST_DOMAIN`,
`OPENSANDBOX_TEST_API_KEY`, `OPENSANDBOX_SANDBOX_DEFAULT_IMAGE` and a unique
`OPENSANDBOX_RUST_RUN_ID`. The full suite also needs
`OPENSANDBOX_RUST_VAULT_TARGET_IP` reachable from sandbox networking. Interpreter
scenarios additionally require a running Jupyter service. Set
`OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` only for the included
fixture's startup command. The runner always executes both direct and proxy
modes, including when an existing server is supplied. To choose a single mode,
run a specific ignored Cargo test with `OPENSANDBOX_TEST_USE_SERVER_PROXY` instead.
Existing-server mode does not provision or reconfigure that server.

Overlay tests require namespace and overlay support. Interpreter tests retain
strict state, rich-output, error, interruption and reuse assertions; their
[runtime requirements](#runtime-and-readiness) and [interpreter diagnostics](#interpreter-tests) explain
known compatibility failures. Test coverage does not imply runtime acceptance.

### Interpreter tests

```bash
OPENSANDBOX_RUST_INTERPRETER_ONLY=true bash scripts/rust-e2e.sh
```

The default selection runs all 13 interpreter scenarios in both endpoint modes:
compatibility and stock-`python3` kernels, state, HTML/JSON, kernel errors,
interruption, immediate reuse and raw HTTP/SSE checks. Set
`OPENSANDBOX_RUST_INTERPRETER_FILTER` to a test-name substring for a focused run.
Phases run independently; a passing phase does not establish full acceptance.
The SDK reuse scenarios assert stdout/error and the next successful execution
for 100 consecutive rounds. Assertions are not relaxed, and executions are not
retried to hide failures.

The printed private artifact directory retains per-mode test logs, runtime
metadata and JSON traces under `interpreter/`. Raw traces describe their own
requests, not an earlier SDK request, and omit code, output text, headers, URLs
and environment values; only allowlisted error classifications remain readable.
They report truncation and capture at most 64 KiB, 64 frames and 4 KiB per frame.
Raw assertions reject responses over 1 MiB. Failed expanded scenarios try to
retain `/tmp/execd.log` and, for a local Unix-socket Docker daemon, the sandbox's
last 300 container-log lines. These raw files stay outside `interpreter/`, and
`execd-failure-*.log` / `private-container-failure-*.log` are excluded from CI
upload. Vault diagnostics record allowlisted request stages, elapsed milliseconds
and error classes, without payloads or arbitrary error messages. Each Vault
scenario repeats the complete lifecycle in ten fresh sandboxes; stale revisions
must return HTTP 409, so a timeout cannot satisfy the conflict assertion.

Only the JSON diagnostics follow this redaction policy. Raw server/container/test
logs and configuration may contain credentials or execution content; review
before sharing. The full Rust E2E job uploads test/service logs and the redacted
`interpreter/*.json` traces on success or failure, with five-day retention.
Private configuration is excluded. CI configuration is not evidence of a
completed remote run.

### Kubernetes and VM tests

`bash scripts/rust-k8s-e2e.sh` requires kind, Docker, kubectl and Helm. It creates
its own cluster, scratch `KUBECONFIG`, registry and signed URI gateway. It checks
signed routing, secure access, pause/resume persistence and rootfs snapshot
create/restore/delete, retains diagnostics and deletes its cluster. Even when
those checks pass, it exits with status **2** because VM template acceptance is
not covered. Rootfs snapshots and fast-sandbox VM templates are distinct.

For an existing dedicated cluster, run the tests directly instead of provisioning
another cluster. Supply the endpoint and credentials as for existing-server tests:

```bash
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_secure_signed_route -- --exact --ignored --nocapture
```

This scenario allocates and deletes its own sandbox. Snapshot-capable runtimes
can also run `k8s_secure_signed_pause_snapshot_restore`. Complete template and
environment-variable acceptance requires a dedicated fast-sandbox runtime,
template builder, kernel, artifact storage and KVM:

```bash
OPENSANDBOX_RUST_TEMPLATE_PUBLISH=your-dedicated-artifact-prefix \
  cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_template_create_ready_instantiate_delete -- --exact --ignored --nocapture
```

The template scenario checks create/get/list environment echoes, literal guest
values, inherited `PATH` override and owned-resource cleanup. These scenarios
were not rerun by the focused UID/GID, argv and Redis/TLS validation recorded below.

## Recorded live results

The local 2026-10-13 validation used Rust 1.85.1 and 1.97.1, a macOS caller and
an OrbStack Linux Docker daemon (aarch64, kernel
`7.0.14-orbstack-00380-ga7e0a2dc9535`). Execd and the lifecycle server were
unchanged from `c8eadfd6`. The Python 3.11 workload used Jupyter Server 2.17.0
and ipykernel 6.30.1. Image digests were:

- Execd: `sha256:504d981784fa6e8f29305eb98edead12c7aa027a9ba119d2b8da9783b136aaec`
- Workload: `sha256:47c45426734879a7a3c7b57334cbd77b1998e5e4449c260db0d71929b57540eb`

### Identity, argv and Redis/TLS results

Both toolchains passed the identity and argv scenarios in direct and proxy
modes as part of the full sandbox suite below. Dedicated Redis integration
passed on both toolchains:

| Check | Result per toolchain |
| --- | --- |
| Plaintext Redis concurrency, leases, TTL and destruction | 4 tests passed |
| Authenticated TLS pool operations | 4 tests passed |
| Trusted CA, untrusted CA, wrong hostname and wrong password | 4 child-process cases passed |

### Sandbox results after generated-client migration

Each toolchain passed eight scenarios in both direct and proxy modes:

| Scenario | Result per toolchain and mode |
| --- | --- |
| Namespace, trusted gate and actual overlay prerequisites | 1 passed |
| Lifecycle, shell commands, files, metrics and egress | 1 passed |
| Acquired pool ownership surviving pool shutdown | 1 passed |
| Vault injection, write-only metadata and revision conflicts | 1 passed |
| UID/GID files and native argv, including existing API compatibility | 2 passed |
| Isolated foreground/background execution and scoped files | 1 passed |
| Persistent/ephemeral overlay, writable/read-only binds and advanced run_once | 1 passed |

Total: 32 passed, including 40 complete Vault lifecycle cycles and 40
stale-revision HTTP 409 responses. Interpreter tests were excluded by the
explicit runner switch. No execution retries, longer request timeouts or relaxed
assertions were used. An earlier invocation lost its local workload image;
those image-pull failures were retained as environment failures. Successful
reruns held image references for their duration.

The isolation fixtures set `bootstrap.execd.isolation=enable` and validate
actual namespace/overlay operations. The SDK accepts absent/null command-error
tracebacks as empty vectors, while retaining execution failures and rejecting
malformed traceback values. Public API types remain unchanged.

### Interpreter results

The focused stable-Rust HTML comparison failed in both modes: two SDK tests and
two raw HTTP/SSE tests, four failures total. Raw direct responses omitted rich
HTML before completion; raw proxy responses also contained events after
`execution_complete`. These raw checks bypass SDK decoding. Stock `python3`
selection, completion ordering and immediate reuse also remain known
unmodified-runtime compatibility limitations. Readiness alone is insufficient.

The original three intermittent protocol failures, a later EOF without a
terminal event and the original Vault timeout remain unattributed. A separate
Vault startup failure was traced to a Docker sidecar host-port collision, but
that does not establish the cause of the original timeout. No SDK workaround
or runtime/server change is included. Experimental modified-runtime passes
are not acceptance of this unmodified runtime.

HTTP regression tests cover UTF-8/CRLF chunking, rich MIME preservation,
error-only terminals, reused streaming connections outliving ordinary request
deadlines, and Vault body timeouts without replaying mutations. The 49-check
MSRV/stable SDK matrix passed; that does not establish interpreter stability.

### VM validation scope

Remote CI execution and VM template acceptance remain unverified. These local
Docker runs did not exercise a dedicated fast-sandbox/KVM environment. Kind
snapshot coverage does not substitute for the
[VM template scenario](#kubernetes-and-vm-tests).
