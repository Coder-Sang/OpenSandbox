# Rust real-runtime E2E tests

`bash scripts/rust-e2e.sh` starts an isolated Docker server with configuration and logs in external scratch. It builds a small dedicated Python/Jupyter image, execd and egress, then runs the basic scenario and all `docker_` scenarios in both direct and server-proxy modes. The credential target accepts only a synthetic test token and never echoes credentials. No user's existing containers are removed.

Coverage includes command failures, binary/text files, metrics, egress, isolated foreground/background runs and log cursors, scoped file confinement, vault injection/revision conflicts/write-only metadata, persistent interpreter contexts, HTML/JSON outputs, kernel errors and interruption, and acquired pool ownership surviving pool shutdown.

For an existing dedicated server, supply `OPENSANDBOX_TEST_DOMAIN`, `OPENSANDBOX_TEST_API_KEY`, `OPENSANDBOX_SANDBOX_DEFAULT_IMAGE`, a unique `OPENSANDBOX_RUST_RUN_ID` and `OPENSANDBOX_RUST_VAULT_TARGET_IP`. The interpreter image must run Jupyter. `OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` selects the included fixture's startup command. Set `OPENSANDBOX_TEST_USE_SERVER_PROXY=true` to select proxy mode. Explicit tests fail when required configuration or runtime features are unavailable, rather than claiming a pass.

`PATH=/path/to/scratch/bin:$PATH bash scripts/rust-k8s-e2e.sh` requires kind, Docker, kubectl and Helm. It creates a uniquely named Kind cluster and a separate scratch `KUBECONFIG`, builds controller/runtime images, deploys a cluster-owned registry, configures containerd HTTP registry pulls, and deploys a signed URI gateway. It checks signed route acceptance/tamper denial, secure access, rootfs persistence across pause/resume, and snapshot create/Ready/restore/delete. Cleanup deletes only the invocation's cluster, after retaining diagnostics. It never consumes the active user's Kubernetes cluster.

For an already authorized cluster, do not run the Kind provisioning script. The Kubernetes tests accept `OPENSANDBOX_TEST_API_KEY` (fallback: `kubernetes-e2e`). Signed/secure routing can be checked independently of pause and snapshot support:

```bash
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_secure_signed_route -- --exact --ignored --nocapture
```

This scenario creates and deletes its own sandbox, so run it only when sandbox allocation is authorized. It does not configure the cluster. The existing combined `k8s_secure_signed_pause_snapshot_restore` scenario remains available for snapshot-capable runtimes.

The `expanded.rs` tests retain their historical `docker_` names but use runtime-neutral SDK APIs. An existing Kubernetes server can run them when its image and runtime provide the required features. The vault target must be reachable from sandbox networking, not merely from the test runner. The dedicated Jupyter fixture uses `JUPYTER_HOST=http://127.0.0.1:44771` and the matching synthetic token. Set `OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` to supply its explicit entrypoint: the SDK's default image entrypoint is `tail -f /dev/null`, not the image's Jupyter CMD. Without this override, merely selecting the fixture image does not start Jupyter. The fixture intentionally also installs a kernelspec named `python` for compatibility with older released execd runtimes that skip the stock `python3` name even when its declared language is `python`. The current source runtime contains an unreleased fix: it prefers other matching spec names but falls back to the standard `python3` spec when that is the only match. Registering `python` remains useful when running the fixture against older releases, but it is no longer required by the fixed source runtime. Execd context creation retries unavailable Jupyter sessions and can outlast the client request timeout. A context-creation timeout occurs before any `/code` execution mapping is exercised.

The fixture's compatibility kernel registration resolves context provisioning only, not full interpreter acceptance. The live interpreter scenario intentionally continues to require persistent state, HTML/JSON output, kernel errors and interruption. The current source runtime includes unreleased stream fixes that correlate Jupyter messages to the execution request, forward `display_data` MIME bundles as existing SSE `result` events even without a positive count, and wait for both `execute_reply` and IOPub `idle` before finalizing. Successful execution ends with `execution_complete`; failure ends with `error`, not an additional success completion. Older execd images that omit streaming `display_data` handling or emit `execution_complete` before all execution results are delivered will fail this scenario. Use an execd build containing the fixes and retain the complete scenario assertions. Do not replace those checks with sleeps or claim interpreter acceptance from a successful context creation alone.

The `docker_interpreter_standard_python3_context_rich_errors_interrupt` scenario removes the fixture's compatibility kernelspec only inside its newly created, owned sandbox, then runs the same persistent-state, rich-output, error, interruption and context-reuse checks with the stock `python3` spec. Run it alongside `docker_interpreter_context_rich_errors_interrupt` in direct and server-proxy modes. Both require an updated execd containing these fixes, not the stock release 1.1.0 runtime.

The interpreter scenarios also inspect raw HTTP events, requiring HTML/JSON and stdout before the terminal marker with no later execution output. They repeat error-then-immediate-reuse 20 times in each owned context, checking that persistent state and results survive without retries. The dedicated fixture writes execd logs to `/tmp/execd.log` for failure diagnostics. A failing run remains a failure even if a later invocation succeeds.

BatchSandbox rootfs snapshots are distinct from fast-sandbox VM templates. The Kind runner exits with status 2 after its supported workflow to explicitly report that template acceptance is unavailable. Template construction requires a separately provisioned dedicated fast-sandbox runtime, template builder with kernel, artifact storage and KVM support. Run its complete owned create/Succeeded/instantiate/delete scenario there:

```bash
export OPENSANDBOX_RUST_TEMPLATE_PUBLISH=your-dedicated-artifact-prefix
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_template_create_ready_instantiate_delete -- --ignored --nocapture
```

All live scenarios are marked ignored so ordinary `cargo test` never allocates resources. Scripts explicitly execute their applicable scenarios. Legacy source-only tests in `e2e.rs` remain available, but do not replace the owned creation workflows.
