# Dynamic PVC selection with a bwrap Pool

`bwrap-v1` Pools mount a PVC once while Pods are pre-warmed. A Create Sandbox
request then selects relative directories from named, administrator-controlled
roots. The selected directories are exposed only inside that allocation's
long-lived bubblewrap runtime.

Keep the hardening policy in a versioned immutable ConfigMap owned by the Pool
operator. This policy is not part of Create Sandbox and is never selected by a
workload:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: bwrap-isolation-v1
immutable: true
data:
  isolation.toml: |
    allowed_writable = ["/workspace", "/run", "/tmp"]

    [hardening]
    enabled = true
    keep_capabilities = []
    allow_seccomp_user_notification = false

    [landlock]
    enabled = true
    allow_private_proc_read = false
```

`allow_seccomp_user_notification` permits a process-observation workload to
install a stricter nested seccomp filter; the outer execd filter remains in
force. `allow_private_proc_read` is intentionally disabled by default because
it expands Landlock visibility from `/proc/self` to the Pool runtime's private
PID namespace. A Pool enabling it must also protect its PID-1 anchor from
same-UID memory inspection and repeat the attack matrix for that image and
kernel. Execd accepts this setting only for forced bwrap Pools and grants
read-only access to the private procfs. It validates and pins the Pool PID-1
anchor while the native gate is inspectable, then waits for the gate to become
non-dumpable before marking the runtime ready. The gate remains the native
anchor instead of executing a shell, which would reset that protection. It
never grants access to the outer execd/task-executor procfs or adds
`CAP_SYS_PTRACE`.

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
          env:
            - name: EXECD_ISOLATION_CONFIG
              value: /etc/execd/isolation.toml
          volumeMounts:
            - name: storage
              mountPath: /storage
            - name: isolation-config
              mountPath: /etc/execd/isolation.toml
              subPath: isolation.toml
              readOnly: true
      volumes:
        - name: storage
          persistentVolumeClaim:
            claimName: sandbox-workspaces
        - name: isolation-config
          configMap:
            name: bwrap-isolation-v1
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
`opensandbox-nsenter`. The kernel must support seccomp, `openat2`, cgroup
namespaces, and descriptor-based bind mounts. Execd chooses the UID mode at
startup: it prefers a nested user namespace and falls back to real UID/GID 0
with `setpriv` when nested user namespaces are unavailable. If neither mode is
available, initialization fails closed.

When an initContainer installs execd into a shared volume, install
`/opt/opensandbox/opensandbox-session-gate` from the **same execd image** as
`/opt/opensandbox/execd`. The Pool anchor handshake requires matching
versions of both binaries; updating only execd will fail closed.

The trusted execd container is not privileged. It drops all capabilities and
adds only `SYS_ADMIN`, `SYS_CHROOT`, `SETPCAP`, and `SETGID`, with Pod-level
seccomp and AppArmor set to `Unconfined` for the supervisor. These privileges
are required to construct and re-enter the namespaces; the hardening launcher
then clears the inheritable, permitted, effective, bounding, and ambient
capability sets before starting any user process, sets `no_new_privs`, and
installs the built-in seccomp policy. Landlock is mandatory when the kernel
advertises support; only an explicit `unsupported` result may degrade.

Create a sandbox by selecting directories, never by sending source paths:

```json
{
  "extensions": {"poolRef": "bwrap-workspaces"},
  "isolation": {
    "type": "bwrap",
    "mounts": [
      {"root": "projects", "subPath": "project-A", "target": "/workspace/a", "mode": "rw"},
      {"root": "shared", "subPath": "sdk", "target": "/workspace/sdk", "mode": "ro"}
    ]
  }
}
```

The API rejects traversal, absolute `subPath` values, duplicate targets,
targets outside the declared prefixes, permission escalation beyond each
root's `maxMode`, non-Delete recycling, and use with an ordinary Pool. Nested
targets are mounted parent-first regardless of request order, and the child
mount replaces the corresponding subtree of its parent. A separately
authorized `rw` child may be nested below a `ro` parent. Missing nested target
directories are created with mode `0755` only through a writable parent and
remain on the PVC; a read-only parent requires the directory to exist.
Failure to establish or verify the bwrap runtime leaves execd unready and the
Pool Pod is deleted; it never falls back to direct execution.

The selected UID mode is exposed as `uidMode` by `/ready` and as
`pool_uid_mode` by the capabilities endpoint. In `setpriv` mode the user
namespace inode is expected to match the outer container; the mount, PID, IPC,
UTS, and cgroup namespaces must still differ, and user processes must report
all five capability sets as zero.

For this Pool type, execd business endpoints are always authenticated even if
the create request omits `secureAccess`. The lifecycle service returns the
required `X-EXECD-ACCESS-TOKEN` through normal endpoint headers; the raw token
is not copied into the task environment or the bwrap runtime.

The bwrap runtime is one-shot. If it exits unexpectedly, execd first marks the
runtime unhealthy and rejects all business operations, then exits with a
failure status. The corresponding BatchSandbox task uses
`taskResourcePolicyWhenCompleted: Release`; the Pool's Delete recycler removes
the unusable Pod and replenishes its configured buffer. The failed
BatchSandbox remains available for status inspection until its expiration or
explicit deletion. PVC contents are retained, but changes to the deleted Pod's
container root are discarded.
