---
title: Kotlin/Java SDK
description: Kotlin SDK for creating, managing, and interacting with secure OpenSandbox environments.
---

# OpenSandbox SDK for Kotlin/Java

A Kotlin SDK for low-level interaction with OpenSandbox. It provides capabilities to create, manage, and interact with secure sandbox environments, including executing shell commands, managing files, and monitoring resources.

## Installation

### Gradle (Kotlin DSL)

```kotlin
dependencies {
    implementation("com.alibaba.opensandbox:sandbox:{latest_version}")
}
```

### Maven

```xml
<dependency>
    <groupId>com.alibaba.opensandbox</groupId>
    <artifactId>sandbox</artifactId>
    <version>{latest_version}</version>
</dependency>
```

## Quick Start

The following example shows how to create a sandbox and execute a shell command.

::: tip
Before running this example, ensure the OpenSandbox service is running. See the [Getting Started](/getting-started/) guide for startup instructions.
:::

```java
import com.alibaba.opensandbox.sandbox.Sandbox;
import com.alibaba.opensandbox.sandbox.config.ConnectionConfig;
import com.alibaba.opensandbox.sandbox.domain.exceptions.SandboxException;
import com.alibaba.opensandbox.sandbox.domain.models.execd.executions.Execution;

public class QuickStart {
    public static void main(String[] args) {
        // 1. Configure connection
        ConnectionConfig config = ConnectionConfig.builder()
            .domain("api.opensandbox.io")
            .apiKey("your-api-key")
            .build();

        // 2. Create a Sandbox using try-with-resources
        try (Sandbox sandbox = Sandbox.builder()
                .connectionConfig(config)
                .image("ubuntu")
                .build()) {

            try {
                Execution execution = sandbox.commands().run("echo 'Hello Sandbox!'");
                System.out.println(execution.getLogs().getStdout().get(0).getText());
            } finally {
                sandbox.kill();
            } // try-with-resources closes the client even if kill() fails.

        } catch (SandboxException e) {
            // Handle Sandbox specific exceptions
            System.err.println("Sandbox Error: [" + e.getError().getCode() + "] " + e.getError().getMessage());
            System.err.println("Request ID: " + e.getRequestId());
        } catch (Exception e) {
            e.printStackTrace();
        }
    }
}
```

## Lifecycle Hooks

Configure lifecycle hooks on `Sandbox.Builder`. `preStart` completes before the entrypoint starts, while `periodic` hooks run on their schedules after startup.

```java
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.LifecycleHook;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.PeriodicLifecycleHook;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.SandboxLifecycle;

SandboxLifecycle lifecycle = SandboxLifecycle.builder()
    .preStart(LifecycleHook.builder()
        .command("sh", "-c", "echo ready > /tmp/prestart.done")
        .timeoutSeconds(120)
        .build())
    .periodic(PeriodicLifecycleHook.builder()
        .name("checkpoint")
        .schedule("@every 5m")
        .command("sh", "-c", "date -u >> /tmp/checkpoints.log")
        .timeoutSeconds(120)
        .build())
    .build();

Sandbox sandbox = Sandbox.builder()
    .connectionConfig(config)
    .image("ubuntu:24.04")
    .lifecycle(lifecycle)
    .build();
```

The Server validates `timeoutSeconds`; `preStart` accepts 1–10800 seconds, while `periodic` accepts 1–300 seconds. Both default to 60 seconds when omitted. See [Lifecycle Hooks](/guides/lifecycle-hooks) for timing, failure behavior, and provider limitations.

## Usage Examples

### 1. Lifecycle Management

Manage the sandbox lifecycle, including renewal, pausing, and resuming.

```java
// Renew the sandbox
// This resets the expiration time to (current time + duration)
sandbox.renew(Duration.ofMinutes(30));

// Request pause (runtime-dependent)
sandbox.pause();
```

Pause returns after the request is accepted. Poll sandbox info until the state is
`Paused` before resuming; also handle `Failed` and set a polling deadline. Runtime
behavior is described in [Pause and Resume](/guides/pause-resume). Then resume:

```java

// Resume execution
// There is no Sandbox.resume() instance method: resuming re-attaches to an
// existing sandbox by id and returns a new, connected handle.
Sandbox resumed = Sandbox.resumer()
    .sandboxId(sandbox.getId())
    .connectionConfig(config)
    .resume();

// Get current status
SandboxInfo info = resumed.getInfo();
System.out.println("State: " + info.getStatus().getState());
System.out.println("Expires: " + info.getExpiresAt()); // null when manual cleanup mode is used
```

Create a non-expiring sandbox by passing `timeout(null)`:

```java
Sandbox manual = Sandbox.builder()
    .connectionConfig(config)
    .image("ubuntu")
    .timeout(null)
    .build();
```

### 2. Custom Health Check

Resolving an endpoint confirms that a route exists; it does not confirm that the
application on that port is healthy. For service readiness, make a bounded request
to the application's health endpoint and include the returned endpoint headers.

Define custom logic to determine if the sandbox is healthy. This overrides the default ping check. Set timeouts within custom checks; the SDK cannot interrupt them.

```java
Sandbox sandbox = Sandbox.builder()
    .connectionConfig(config)
    .image("nginx:latest")
    // Custom check: Wait for port 80 to be accessible
    .healthCheck(sbx -> {
        try {
            // 1. Get the external mapped address for port 80
            SandboxEndpoint endpoint = sbx.getEndpoint(80);

            // 2. Perform your connection check (e.g. HTTP request, Socket connect)
            // return checkConnection(endpoint.getEndpoint());
            return true;
        } catch (Exception e) {
            return false;
        }
    })
    .build();
```

### 3. Command Execution & Streaming

Execute commands and handle output streams in real-time.

```java
// Create handlers for streaming output
ExecutionHandlers handlers = ExecutionHandlers.builder()
    .onStdout(msg -> System.out.println("STDOUT: " + msg.getText()))
    .onStderr(msg -> System.err.println("STDERR: " + msg.getText()))
    .onExecutionComplete(complete ->
        System.out.println("Command finished in " + complete.getExecutionTimeInMillis() + "ms")
    )
    .build();

// Execute command with handlers
RunCommandRequest request = RunCommandRequest.builder()
    .command("for i in {1..5}; do echo \"Count $i\"; sleep 0.5; done")
    .handlers(handlers)
    .build();

sandbox.commands().run(request);
```

To execute a native program without shell parsing, pass an argument list. On Linux,
this example prints literal `$HOME` and keeps `hello world` as one argument:

```java
sandbox.commands().run(RunCommandRequest.builder()
    .argv(List.of("printf", "%s\n", "$HOME", "hello world"))
    .build());
```

Native argv execution requires an updated execd. See [command execution modes](/components/execd#command-execution) for executable lookup and platform behavior.

### 4. Comprehensive File Operations

Manage files and directories, including read, write, list, delete, and search.

```java
// 1. Write file
sandbox.files().write(List.of(
    WriteEntry.builder()
        .path("/tmp/hello.txt")
        .data("Hello World")
        .mode(644)
        .build()
));

// 2. Read file
String content = sandbox.files().readFile("/tmp/hello.txt", "UTF-8", null);
System.out.println("Content: " + content);

// 3. List/Search files
List<EntryInfo> files = sandbox.files().search(
    SearchEntry.builder()
        .path("/tmp")
        .pattern("*.txt")
        .build()
);
files.forEach(f -> System.out.println("Found: " + f.getPath()));

// 4. Delete file
sandbox.files().deleteFiles(List.of("/tmp/hello.txt"));
```

### 5. Sandbox Management (Admin)

Use `SandboxManager` for administrative tasks and finding existing sandboxes.

```java
SandboxManager manager = SandboxManager.builder()
    .connectionConfig(config)
    .build();

import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.SandboxState;

// ...

// List running sandboxes
PagedSandboxInfos sandboxes = manager.listSandboxInfos(
    SandboxFilter.builder()
        .states(SandboxState.RUNNING)
        .pageSize(10)
        .page(1)
        .build()
);

sandboxes.getSandboxInfos().forEach(info -> {
    System.out.println("Found sandbox: " + info.getId());
    // Perform admin actions
    manager.killSandbox(info.getId());
});

// Try-with-resources will automatically call manager.close()
// manager.close();
```

### 6. Client Pool and observability

`SandboxPool` provides four acquire policies and staged warmup controls. Use
`InMemoryPoolStateStore` in one process, or the optional
`com.alibaba.opensandbox:sandbox-pool-redis` module with a caller-managed Jedis
client for distributed deployments. See [Client Pool](/guides/client-pool)
for configuration, examples, cleanup, and namespace retirement.

Set `ConnectionConfig.builder().enableTracing(true)` to emit [pool warmup traces](/sdks/observability#pool-warmup-tracing).
The JVM SDK also adds trace IDs to SLF4J MDC. For remote logs/events, use
`SandboxManager.getDiagnosticLogs` / `getDiagnosticEvents`; see [Diagnostics](/api/#diagnostics).
Create-latency reporting is controlled separately by [SDK Telemetry](/sdks/observability#creation-metrics).

## Snapshots and metadata

Create a snapshot with `sandbox.createSnapshot` or `manager.createSnapshot`.
`SandboxManager` exposes `getSnapshot`, `listSnapshots`, `deleteSnapshot`, and
`waitForSnapshotReady` to wait for asynchronous snapshot completion. Restore with
`Sandbox.builder().snapshotId(snapshotId).connectionConfig(config).build()`.

Use `sandbox.patchMetadata` or `manager.patchSandboxMetadata` to add/replace
metadata keys; a `null` value removes a key. Runtime support is described by the
[lifecycle contract](/api/#1-sandbox-lifecycle-yml).

## Configuration

### 1. Connection Configuration

The `ConnectionConfig` class manages API server connection settings.

| Parameter        | Description                                | Default                      | Environment Variable   |
| ---------------- | ------------------------------------------ | ---------------------------- | ---------------------- |
| `apiKey`         | API Key for authentication                 | Optional; needed when server auth is enabled | `OPEN_SANDBOX_API_KEY` |
| `domain`         | The endpoint domain of the sandbox service | `localhost:8080` | `OPEN_SANDBOX_DOMAIN`  |
| `protocol`       | HTTP protocol (http/https)                 | `http`                       | -                      |
| `requestTimeout` | Timeout for API requests                   | 30 seconds                   | -                      |
| `debug`          | Enable debug logging for HTTP requests     | `false`                      | -                      |
| `headers`        | Custom HTTP headers                        | Empty                        | -                      |
| `connectionPool` | Shared OKHttp ConnectionPool               | SDK-created per instance     | -                      |
| `retryPolicy`    | Automatic retry policy for non-streaming requests (see [Automatic retries](#_2-automatic-retries)) | Enabled (`RetryPolicy()`) | -                 |
| `useServerProxy` | Use sandbox server as proxy for execd/endpoint requests (e.g. when client cannot reach the sandbox directly) | `false` | -                      |
| `disableMetrics` | Disable SDK create-latency telemetry (see [SDK Telemetry](/sdks/observability#creation-metrics)) | `false` | `OPENSANDBOX_DISABLE_METRICS` |
| `enableTracing` | Enable OpenTelemetry tracing for pool warmup (see [SDK Tracing](/sdks/observability#pool-warmup-tracing)) | `false` | - |

```java
// 1. Basic configuration
ConnectionConfig config = ConnectionConfig.builder()
    .apiKey("your-key")
    .domain("api.opensandbox.io")
    .requestTimeout(Duration.ofSeconds(60))
    .build();

// 2. Advanced: Shared Connection Pool
// If you create many Sandbox instances, sharing a connection pool is recommended to save resources.
// SDK default keep-alive is 30 seconds for its own pools.
ConnectionPool sharedPool = new ConnectionPool(50, 30, TimeUnit.SECONDS);

ConnectionConfig sharedConfig = ConnectionConfig.builder()
    .apiKey("your-key")
    .domain("api.opensandbox.io")
    .headers(Map.of(
        "X-Custom-Header", "value",
        "X-Request-ID", "trace-123"
    ))
    .connectionPool(sharedPool) // Inject shared pool
    .build();
```

::: tip SDK Telemetry
`Sandbox.builder()...build()` reports create latency to `POST /v1/metrics/events` by default. Call `ConnectionConfig.builder().disableMetrics(true)` or export `OPENSANDBOX_DISABLE_METRICS=1` to opt out. See [SDK Telemetry](/sdks/observability#creation-metrics).
:::

### 2. Automatic retries

The SDK retries transient failures automatically. `ConnectionConfig` installs a
`RetryInterceptor` (`com.alibaba.opensandbox.sandbox.transport.RetryPolicy`) on the
SDK's non-streaming HTTP clients.

Default behavior:

- **Enabled by default.** Idempotent methods (`GET/HEAD/PUT/DELETE/OPTIONS`) are
  retried on `429`, `502`, `503`, and on pre-send transport failures (DNS, TCP
  connect, TLS handshake).
- **`POST`/`PATCH` are never retried on a status code by default**, since the
  request may already have been applied server-side. Pre-send transport failures
  (before any byte is written) are still retried for these methods.
- Up to `3` retries with decorrelated-jitter exponential backoff, honoring a server
  `Retry-After` header (capped at 60s).
- **SSE / streaming requests bypass all automatic retry** because their bodies
  are not safely replayable. The SSE client also disables OkHttp's built-in
  connection recovery to prevent a streaming command POST from being replayed.

::: warning Behavior change
SDK-policy retries are on by default. This can increase the number of HTTP attempts
and tail latency compared to earlier SDK versions. To disable the new SDK-policy
retries, use `RetryPolicy.disabled()`; non-streaming requests then fall back to
OkHttp's pre-existing built-in connection recovery.
:::

```java
import com.alibaba.opensandbox.sandbox.transport.RetryPolicy;
import com.alibaba.opensandbox.sandbox.transport.StatusCode;
import java.time.Duration;
import java.util.Set;

// Disable SDK-policy retries and retain OkHttp's built-in connection recovery.
ConnectionConfig config = ConnectionConfig.builder()
    .apiKey("your-key")
    .domain("api.opensandbox.io")
    .retryPolicy(RetryPolicy.disabled())
    .build();

// Custom policy: more retries, an overall wall-clock deadline, and an opt-in to
// retry POST/PATCH on 503 (only safe if your endpoints are idempotent).
ConnectionConfig tuned = ConnectionConfig.builder()
    .apiKey("your-key")
    .domain("api.opensandbox.io")
    .retryPolicy(new RetryPolicy(
        /* maxRetries */ 5,
        /* initialBackoff */ Duration.ofMillis(500),
        /* maxBackoff */ Duration.ofSeconds(30),
        /* backoffMultiplier */ 2.0,
        /* jitter */ com.alibaba.opensandbox.sandbox.transport.JitterMode.DECORRELATED,
        /* retryableStatusCodesIdempotent */ RetryPolicy.DEFAULT_IDEMPOTENT_STATUS,
        /* retryableStatusCodesNonIdempotent */ Set.of(StatusCode.SERVICE_UNAVAILABLE),
        /* perAttemptTimeout */ null,
        /* overallDeadline */ Duration.ofSeconds(20),
        /* onRetry */ null))
    .build();
```

### 3. Sandbox Creation Configuration

The `Sandbox.builder()` allows configuring the sandbox environment.

| Parameter      | Description                              | Default                         |
| -------------- | ---------------------------------------- | ------------------------------- |
| `image`        | Docker image to use                      | One of image or snapshot ID |
| `timeout`      | Automatic termination timeout            | 10 minutes                      |
| `entrypoint`   | Container entrypoint command             | `["tail", "-f", "/dev/null"]`   |
| `resource`     | CPU and memory limits                    | `{"cpu": "1", "memory": "2Gi"}` |
| `env`          | Environment variables                    | Empty                           |
| `metadata`     | Custom metadata tags                     | Empty                           |
| `extensions`   | Opaque server-side extension parameters  | Empty                           |
| `networkPolicy` | Optional outbound network policy (egress) | -                             |
| `credentialProxy` | Optional Credential Vault proxy startup settings | -                     |
| `readyTimeout` | Max time to wait for sandbox to be ready | 30 seconds                      |
| `snapshotId` | Restore a snapshot instead of an image | - |
| `resourceRequests` | Kubernetes resource requests; must not exceed limits | - |
| `lifecycle` | Pre-start and periodic hooks | - |
| `platform` | OS/architecture constraint | - |
| `volumes` | Host, PVC, or OSSFS mounts | - |
| `secureAccess` | Require endpoint access credentials | `false` |

::: warning
Metadata keys under `opensandbox.io/` are reserved for system-managed labels and will be rejected by the server.
:::

```java
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.NetworkPolicy;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.NetworkRule;

Sandbox sandbox = Sandbox.builder()
    .connectionConfig(config)
    .image("python:3.11")
    .timeout(Duration.ofMinutes(30))
    .resource(Map.of("cpu", "2", "memory", "4Gi"))
    .env("PYTHONPATH", "/app")
    .metadata("project", "demo")
    .extension("storage.id", "dataset-001")
    .networkPolicy(
        NetworkPolicy.builder()
            .defaultAction(NetworkPolicy.DefaultAction.DENY)
            .addEgress(
                NetworkRule.builder()
                    .action(NetworkRule.Action.ALLOW)
                    .target("pypi.org")
                    .build()
            )
            .build()
    )
    .build();
```

### 4. Runtime Egress Policy Updates

Runtime egress reads and patches go directly to the sandbox egress sidecar.
The SDK first resolves the sandbox endpoint on port `18080`, then calls the sidecar `/policy` API.

Template-backed sandboxes have no sandbox-side egress sidecar: the SDK detects
them via the server's `OPEN-SANDBOX-ORIGIN` response header (see
[Fsb Template Management](#fsb-template-management)) and routes the same
`getEgressPolicy` / `patchEgressRules` / `deleteEgressRules` calls through the
lifecycle control plane (`/sandboxes/{sandboxId}/networkpolicy`) instead.

Patch uses merge semantics:
- Incoming rules take priority over existing rules with the same `target`.
- Existing rules for other targets remain unchanged.
- Within a single patch payload, the first rule for a `target` wins.
- The current `defaultAction` is preserved.

```java
NetworkPolicy policy = sandbox.getEgressPolicy();

sandbox.patchEgressRules(
    List.of(
        NetworkRule.builder().action(NetworkRule.Action.ALLOW).target("www.github.com").build(),
        NetworkRule.builder().action(NetworkRule.Action.DENY).target("pypi.org").build()
    )
);
```

### 5. Credential Vault

Credential Vault injects outbound credentials from the egress sidecar while
keeping real secrets out of sandbox environment variables, commands, files, and
logs. Create the sandbox with `credentialProxyEnabled(true)`, then write
credentials and bindings through `sandbox.credentialVault()`.

```java
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.Credential;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.CredentialAuth;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.CredentialBinding;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.CredentialMatch;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.CredentialVaultCreateRequest;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.NetworkPolicy;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.NetworkRule;
import java.util.List;

Sandbox sandbox = Sandbox.builder()
    .connectionConfig(config)
    .image("python:3.11")
    .networkPolicy(
        NetworkPolicy.builder()
            .defaultAction(NetworkPolicy.DefaultAction.DENY)
            .addEgress(
                NetworkRule.builder()
                    .action(NetworkRule.Action.ALLOW)
                    .target("api.example.com")
                    .build()
            )
            .build()
    )
    .credentialProxyEnabled(true)
    .build();

sandbox.credentialVault().create(
    CredentialVaultCreateRequest.builder()
        .credentials(
            List.of(
                Credential.builder()
                    .name("api-token")
                    .inlineSource("<token>")
                    .build()
            )
        )
        .bindings(
            List.of(
                CredentialBinding.builder()
                    .name("api-token")
                    .match(
                        CredentialMatch.builder()
                            .schemes(CredentialMatch.Scheme.HTTPS)
                            .hosts("api.example.com")
                            .paths("/v1/*")
                            .build()
                    )
                    .auth(CredentialAuth.apiKey("x-api-key", "api-token"))
                    .build()
            )
        )
        .build()
);
```

See [Credential Vault](/guides/credential-vault) for auth types, binding
guidance, and Git/curl examples.

::: warning
Credential Vault is unavailable for template-backed sandboxes: they have no
sandbox-side egress sidecar. `sandbox.credentialVault()` throws for them.
:::

## Fsb Template Management

fsb (fast-sandbox microVM) golden-image templates are managed through
`SandboxManager`. Template builds are asynchronous: `createTemplate` returns
with `status.phase` set to `Pending`; poll `getTemplate` until the phase
reaches `Succeeded` or `Failed`. Only a `Succeeded` template can create
sandboxes.

```java
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.CreateTemplateRequest;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.TemplateFilter;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.TemplateInfo;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.TemplatePhase;
import com.alibaba.opensandbox.sandbox.domain.models.sandboxes.TemplateReadiness;

CreateTemplateRequest request = CreateTemplateRequest.builder()
    .image("alpine:3.19")
    .publish("s3://bucket/publish")
    .resourceLimits(Map.of("cpu", "1", "memory", "512Mi", "disk", "2Gi"))
    .readiness(TemplateReadiness.builder().probe("tcp://127.0.0.1:44772").build())
    .metadata("team", "backend")
    .build();

// Start the async build (starts at TemplatePhase.PENDING)
TemplateInfo template = manager.createTemplate(request);

// Poll until the build finishes
while (!template.getStatus().getPhase().equals(TemplatePhase.SUCCEEDED)
    && !template.getStatus().getPhase().equals(TemplatePhase.FAILED)) {
    Thread.sleep(2000);
    template = manager.getTemplate(template.getTemplateId());
}

// List with metadata filters (1-indexed paging)
manager.listTemplates(
    TemplateFilter.builder()
        .metadata(Map.of("team", "backend"))
        .pageSize(20)
        .page(1)
        .build()
);

// Delete a template. Sandboxes already created from it are unaffected.
manager.deleteTemplate(template.getTemplateId());
```

### Creating a Sandbox from a Template

Use `Sandbox.fromTemplate()` to create a sandbox from a `Succeeded` template.
Template mode fixes the workload shape on the server: only `metadata`,
`networkPolicy` and `extensions` may accompany the template id, and `timeout`
is required.

```java
import java.time.Duration;

Sandbox sandbox = Sandbox.fromTemplate()
    .connectionConfig(config)
    .templateId("tpl_123")
    .timeout(Duration.ofMinutes(30))
    .metadata("project", "demo")
    .networkPolicy(
        NetworkPolicy.builder()
            .defaultAction(NetworkPolicy.DefaultAction.DENY)
            .addEgress(
                NetworkRule.builder()
                    .action(NetworkRule.Action.ALLOW)
                    .target("pypi.org")
                    .build()
            )
            .build()
    )
    .create();
```

The created sandbox reports `SandboxOrigin.TEMPLATE` from `sandbox.getOrigin()`.
Template-backed sandboxes have no egress sidecar, so the SDK routes their
egress policy through the lifecycle control plane automatically — including
`Sandbox.connector()` and `Sandbox.resumer()` re-attach flows, which detect
the origin from the server's `OPEN-SANDBOX-ORIGIN` response header.
