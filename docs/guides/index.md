---
title: Feature Guides
description: Guides to faster sandbox acquisition, SDK observability, and sandbox security.
---

# Feature Guides

Use these guides to add capabilities to a working [sandbox application](/getting-started/).
Check the [SDK capability matrix](/sdks/#capability-coverage) for language support.

## Startup latency and observability

| Guide | Use it to | SDK support |
| --- | --- | --- |
| [Client Pool](/guides/client-pool) | Acquire from a pre-warmed buffer and replenish it in the background | Python, JavaScript, Kotlin/Java, Go |
| [SDK Observability](/sdks/observability) | Configure pool traces and creation metrics, and investigate startup latency | Metrics: all five SDKs; traces: Python, JavaScript, Kotlin/Java |

Client Pool tracing is opt-in. Create-latency telemetry is enabled by default and
can be disabled independently. Neither replaces sandbox CPU/memory metrics or
remote diagnostic logs.

## Lifecycle and security

- [Pause and resume](/guides/pause-resume)
- [Lifecycle hooks](/guides/lifecycle-hooks)
- [Isolation sessions](/guides/isolation-sessions)
- [Credential Vault](/guides/credential-vault)
- [Secure access](/guides/secure-access)
- [Secure container runtimes](/guides/secure-container)
- [Multi-tenancy](/guides/multi-tenancy)
- [Windows sandboxes](/guides/windows-sandbox)
