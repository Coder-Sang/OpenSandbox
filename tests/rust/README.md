# Rust real-runtime E2E tests

`bash scripts/rust-e2e.sh` starts an isolated Docker server with configuration and logs in external scratch. It builds a small dedicated Python/Jupyter image, execd and egress, then runs the basic scenario and all `docker_` scenarios in both direct and server-proxy modes. The credential target accepts only a synthetic test token and never echoes credentials. No user's existing containers are removed. Its host-run server uses `proxy.resolve_internal=false` in the generated scratch configuration so server-proxy mode targets Docker's published ports, not bridge IPs that are unreachable from a macOS host.

Coverage includes command failures, binary/text files, metrics, egress, isolated foreground/background runs and log cursors, scoped file confinement, vault injection/revision conflicts/write-only metadata, persistent interpreter contexts, HTML/JSON outputs, kernel errors and interruption, and acquired pool ownership surviving pool shutdown.

For an existing dedicated server, supply `OPENSANDBOX_TEST_DOMAIN`, `OPENSANDBOX_TEST_API_KEY`, `OPENSANDBOX_SANDBOX_DEFAULT_IMAGE`, a unique `OPENSANDBOX_RUST_RUN_ID` and `OPENSANDBOX_RUST_VAULT_TARGET_IP`. The interpreter image must run Jupyter. `OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` selects the included fixture's startup command. Set `OPENSANDBOX_TEST_USE_SERVER_PROXY=true` to select proxy mode. Explicit tests fail when required configuration or runtime features are unavailable, rather than claiming a pass.

`PATH=/path/to/scratch/bin:$PATH bash scripts/rust-k8s-e2e.sh` requires kind, Docker, kubectl and Helm. It creates a uniquely named Kind cluster and a separate scratch `KUBECONFIG`, builds controller/runtime images, deploys a cluster-owned registry, configures containerd HTTP registry pulls, and deploys a signed URI gateway. It checks signed route acceptance/tamper denial, secure access, rootfs persistence across pause/resume, and snapshot create/Ready/restore/delete. Cleanup deletes only the invocation's cluster, after retaining diagnostics. It never consumes the active user's Kubernetes cluster.

For an already authorized cluster, do not run the Kind provisioning script. The Kubernetes tests accept `OPENSANDBOX_TEST_API_KEY` (fallback: `kubernetes-e2e`). Signed/secure routing can be checked independently of pause and snapshot support:

```bash
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_secure_signed_route -- --exact --ignored --nocapture
```

This scenario creates and deletes its own sandbox, so run it only when sandbox allocation is authorized. It does not configure the cluster. The existing combined `k8s_secure_signed_pause_snapshot_restore` scenario remains available for snapshot-capable runtimes.

The `expanded.rs` tests retain their historical `docker_` names but use runtime-neutral SDK APIs. An existing Kubernetes server can run them when its image and runtime provide the required features. The vault target must be reachable from sandbox networking, not merely from the test runner. The dedicated Jupyter fixture uses `JUPYTER_HOST=http://127.0.0.1:44771` and the matching synthetic token. Set `OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` to supply its explicit entrypoint: the SDK's default image entrypoint is `tail -f /dev/null`, not the image's Jupyter CMD. Without this override, merely selecting the fixture image does not start Jupyter. The fixture intentionally installs a kernelspec named `python`: the execd source on this branch skips the stock `python3` name even when its declared language is `python`. An image with only the stock ipykernel spec cannot create this context with that runtime. This Rust SDK branch does not change kernel selection. Execd context creation retries unavailable Jupyter sessions and can outlast the client request timeout. A context-creation timeout occurs before any `/code` execution mapping is exercised.

The fixture's compatibility kernel registration resolves context provisioning only, not full interpreter acceptance. The live interpreter scenario intentionally continues to require persistent state, HTML/JSON output, kernel errors and interruption. Execd runtimes that omit streaming `display_data` handling or emit `execution_complete` before all execution results are delivered will fail this scenario. Runtime fixes are separate from this Rust SDK branch. The strict assertions remain intact, but full interpreter acceptance is not established against the official 1.1.1 runtime. Do not replace those checks with sleeps or claim interpreter acceptance from a successful context creation alone.

The `docker_interpreter_standard_python3_context_rich_errors_interrupt` scenario removes the fixture's compatibility kernelspec only inside its newly created, owned sandbox, then runs the same persistent-state, rich-output, error, interruption and context-reuse checks with the stock `python3` spec. It is a strict compatibility check for a separately supplied runtime supporting that spec. It will fail context provisioning with this branch's unchanged execd selector, and remains explicitly ignored unless requested like the other live scenarios. When evaluating a compatible runtime, run both interpreter scenarios in direct and server-proxy modes. No runtime fix or deployed-runtime acceptance is implied by the SDK test code.

The interpreter scenarios also inspect raw HTTP events, requiring HTML/JSON and stdout before the terminal marker with no later execution output. They repeat error-then-immediate-reuse 20 times in each owned context, checking that persistent state and results survive without retries. The dedicated fixture writes execd logs to `/tmp/execd.log` for failure diagnostics. A failing run remains a failure even if a later invocation succeeds.

BatchSandbox rootfs snapshots are distinct from fast-sandbox VM templates. The Kind runner exits with status 2 after its supported workflow to explicitly report that template acceptance is unavailable. Template construction requires a separately provisioned dedicated fast-sandbox runtime, template builder with kernel, artifact storage and KVM support. Run its complete owned create/Succeeded/instantiate/delete scenario there:

```bash
export OPENSANDBOX_RUST_TEMPLATE_PUBLISH=your-dedicated-artifact-prefix
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_template_create_ready_instantiate_delete -- --ignored --nocapture
```

All live scenarios are marked ignored so ordinary `cargo test` never allocates resources. Scripts explicitly execute their applicable scenarios. Legacy source-only tests in `e2e.rs` remain available, but do not replace the owned creation workflows.

## Official 1.1.1 compatibility run

The Rust branch includes the official `release-1.1.1` base at
`531700ca18a7fa5fab6a2f25d6dc68a76eddab12`. To test published runtime images
instead of locally built components, use the digests from
[`docs/releases/1.1.1.yaml`](../../docs/releases/1.1.1.yaml):

```bash
export OPENSANDBOX_RUST_SKIP_IMAGE_BUILD=1
export OPENSANDBOX_RUST_EXECD_IMAGE=opensandbox/execd@sha256:cda2bc35409bbf57561599b316cafb899598f7dce671332e7ceda14eb1cfeded
export OPENSANDBOX_RUST_EGRESS_IMAGE=opensandbox/egress@sha256:bce2cc2e4dc3a3594678b91ac29efb0669fccebfdfc137727018d3b087fbb13a
RUSTUP_TOOLCHAIN=1.85.1 bash scripts/rust-e2e.sh
```

The isolated macOS Docker run used the release server source, the BOM images
above and the dedicated Jupyter fixture. After correcting the host-run server's
scratch proxy configuration to use published endpoints, its strict results were:

| Scenario | Direct | Server proxy |
| --- | --- | --- |
| Lifecycle, commands, files, metrics and egress | Passed | Passed |
| Acquired pool ownership after shutdown | Passed | Passed |
| Vault injection, revisions and write-only metadata | Passed | Passed |
| Compatibility `python` interpreter | Failed during state initialization: event JSON missing `type` | Failed persistent-state stdout assertion with an empty completed execution |
| Stock `python3` interpreter | Context request timed out | Context request timed out |
| Isolated execution | Blocked by namespace prerequisites | Blocked by namespace prerequisites |

The first run, before correcting proxy endpoint resolution, passed the direct
interpreter's state checks and failed `HTML result lost` with an empty completed
execution. Its first proxy interpreter failed sandbox readiness with repeated
502 responses from unreachable bridge endpoints. The run was then stopped,
so remaining proxy scenarios from that invocation are not counted as executed.
Both runs remain failed. The fixture contains `setpriv`; execd's capability smoke
test failed because namespace creation returned `Operation not permitted`, not
because the executable was absent. No runtime fix or test assertion relaxation
was applied.

These failures do not establish later interpreter JSON/error/interruption/reuse
acceptance. Kubernetes, signed ingress, snapshots, VM templates and live Redis
were not exercised in this 1.1.1 run. No stream EOF failure was observed here,
and historical EKS EOF reports remain separately unresolved.
