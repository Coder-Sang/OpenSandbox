# Dynamic PVC selection with a bwrap Pool

`bwrap-v1` Pools mount a PVC once while Pods are pre-warmed. A Create Sandbox
request then selects relative directories from named, administrator-controlled
roots. The selected directories are exposed only inside that allocation's
long-lived bubblewrap runtime.

```yaml
apiVersion: sandbox.opensandbox.io/v1alpha1
kind: Pool
metadata:
  name: bwrap-workspaces
spec:
  recycleStrategy:
    type: Delete
  template:
    metadata:
      annotations:
        opensandbox.io/execution-isolation: bwrap-v1
        opensandbox.io/bwrap-mount-policy: |
          {
            "version": 1,
            "roots": {
              "projects": {
                "mountRoot": "/storage",
                "source": "/storage/projects",
                "targetPrefixes": ["/workspace"],
                "maxMode": "rw"
              },
              "shared": {
                "mountRoot": "/storage",
                "source": "/storage/shared",
                "targetPrefixes": ["/workspace"],
                "maxMode": "ro"
              }
            }
          }
    spec:
      automountServiceAccountToken: false
      containers:
        - name: task-executor
          image: example.invalid/opensandbox-pool-runtime:latest
          volumeMounts:
            - name: storage
              mountPath: /storage
      volumes:
        - name: storage
          persistentVolumeClaim:
            claimName: sandbox-workspaces
  capacitySpec:
    bufferMin: 1
    bufferMax: 2
    poolMin: 1
    poolMax: 10
```

The controller creates a distinct Secret for every warm Pod and injects
`EXECD_ACCESS_TOKEN` and `TASK_EXECUTOR_AUTH_TOKEN`. Do not add a ServiceAccount
token, CRI socket, Docker socket, or host control socket to this template.
The runtime image must contain bubblewrap 0.11 or newer (for FD binds),
`opensandbox-session-gate`, `opensandbox-launcher`, and
`opensandbox-nsenter`. The kernel must support user namespaces, seccomp,
`openat2`, cgroup namespaces, and descriptor-based bind mounts.

Create a sandbox by selecting directories, never by sending source paths:

```json
{
  "extensions": {"poolRef": "bwrap-workspaces"},
  "entrypoint": ["python", "/app/main.py"],
  "isolation": {
    "type": "bwrap",
    "mounts": [
      {"root": "projects", "subPath": "project-A", "target": "/workspace/a", "mode": "rw"},
      {"root": "shared", "subPath": "sdk", "target": "/workspace/sdk", "mode": "ro"}
    ]
  }
}
```

The API rejects traversal, absolute `subPath` values, overlapping targets,
targets outside the declared prefixes, permission escalation, non-Delete
recycling, and use with an ordinary Pool. Failure to establish the bwrap
runtime leaves execd unready and the Pool Pod is deleted; it never falls back
to direct execution.

For this Pool type, execd business endpoints are always authenticated even if
the create request omits `secureAccess`. The lifecycle service returns the
required `X-EXECD-ACCESS-TOKEN` through normal endpoint headers; the raw token
is not copied into the task environment or the bwrap runtime.
