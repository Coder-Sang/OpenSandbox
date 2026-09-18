---
title: Sandbox Diagnostics
description: Retrieve sandbox diagnostic logs and events through the lifecycle API, Python, Kotlin/Java, or CLI.
---

# Sandbox Diagnostics

Diagnostics retrieve best-effort remote logs and events by sandbox ID. Use a
manager or the CLI when sandbox creation has stalled: these requests go through
the lifecycle server and do not require a working execd connection.

Python (async and sync) and Kotlin/Java expose diagnostics on both `Sandbox` and
`SandboxManager`. JavaScript, Go, and C# do not currently expose this stable SDK
surface; use the CLI or the [diagnostics HTTP API](/api/#2-diagnostic-api-yml).
C# `SdkDiagnosticsOptions` configures local SDK logging, not remote diagnostics.

## Python

```python
import asyncio

from opensandbox import SandboxManager
from opensandbox.config import ConnectionConfig


async def main() -> None:
    async with await SandboxManager.create(
        connection_config=ConnectionConfig(domain="localhost:8080")
    ) as manager:
        result = await manager.get_diagnostic_events("<sandbox-id>", scope="runtime")
        if result.delivery == "inline":
            print(result.content or "")
        else:
            print(result.content_url)
        print("truncated:", result.truncated)
        for warning in result.warnings or []:
            print(warning)


if __name__ == "__main__":
    asyncio.run(main())
```

For synchronous applications, use `SandboxManagerSync` and `ConnectionConfigSync`
with the same method names, without `await`. On an existing sandbox, call
`sandbox.get_diagnostic_logs(scope="container")` or
`sandbox.get_diagnostic_events(scope="runtime")`.

## Kotlin / Java

With an existing `SandboxManager`:

```java
var logs = manager.getDiagnosticLogs(sandboxId, "container");
var events = manager.getDiagnosticEvents(sandboxId, "runtime");
if ("inline".equals(logs.getDelivery())) {
    System.out.println(logs.getContent());
} else {
    System.out.println(logs.getContentUrl());
}
```

`Sandbox` exposes `getDiagnosticLogs(scope)` and `getDiagnosticEvents(scope)`.

## CLI

```bash
osb diagnostics logs <sandbox-id> --scope container -o raw
osb diagnostics events <sandbox-id> --scope runtime -o json
```

## Scopes and delivery

`scope` is required. Docker and Kubernetes support `container`/`all` for logs and
`runtime`/`all` for events. The Fast Sandbox backend supports `runtime`/`all` events;
log collection is not implemented there. Scope names are a server contract:
`lifecycle` appearing in an SDK example or type description does not guarantee
backend support. Unsupported scopes return `DIAGNOSTICS_SCOPE_UNSUPPORTED`.

The response is a descriptor, with `delivery` set to `inline` or `url`. For URL
delivery, fetch `contentUrl` before `expiresAt`; the SDK does not download it for
you. Always inspect `truncated` and `warnings` before treating the output as complete.
The API uses camelCase fields; Python models and CLI JSON/YAML output use snake_case
fields such as `content_url` and `expires_at`.

CLI `-o raw` prints inline content or the URL itself. It does not follow the URL.
See the [diagnostic contract](https://github.com/opensandbox-group/OpenSandbox/blob/main/specs/diagnostic-api.yml)
for request parameters, response fields, and errors.
