# Rust real-runtime E2E tests

`bash scripts/rust-e2e.sh` starts an isolated Docker server with configuration and logs in external scratch. It builds a small dedicated Python/Jupyter image, execd and egress, then runs the basic scenario and all `docker_` scenarios in both direct and server-proxy modes. The credential target accepts only a synthetic test token and never echoes credentials. No user's existing containers are removed.

Coverage includes command failures, binary/text files, metrics, egress, isolated foreground/background runs and log cursors, scoped file confinement, vault injection/revision conflicts/write-only metadata, persistent interpreter contexts, HTML/JSON outputs, kernel errors and interruption, and acquired pool ownership surviving pool shutdown.

For an existing dedicated server, supply `OPENSANDBOX_TEST_DOMAIN`, `OPENSANDBOX_TEST_API_KEY`, `OPENSANDBOX_SANDBOX_DEFAULT_IMAGE`, a unique `OPENSANDBOX_RUST_RUN_ID` and `OPENSANDBOX_RUST_VAULT_TARGET_IP`. The interpreter image must run Jupyter. `OPENSANDBOX_RUST_DEDICATED_JUPYTER=true` selects the included fixture's startup command. Set `OPENSANDBOX_TEST_USE_SERVER_PROXY=true` to select proxy mode. Explicit tests fail when required configuration or runtime features are unavailable, rather than claiming a pass.

`PATH=/path/to/scratch/bin:$PATH bash scripts/rust-k8s-e2e.sh` requires kind, Docker, kubectl and Helm. It creates a uniquely named Kind cluster and a separate scratch `KUBECONFIG`, builds controller/runtime images, deploys a cluster-owned registry, configures containerd HTTP registry pulls, and deploys a signed URI gateway. It checks signed route acceptance/tamper denial, secure access, rootfs persistence across pause/resume, and snapshot create/Ready/restore/delete. Cleanup deletes only the invocation's cluster, after retaining diagnostics. It never consumes the active user's Kubernetes cluster.

BatchSandbox rootfs snapshots are distinct from fast-sandbox VM templates. The Kind runner exits with status 2 after its supported workflow to explicitly report that template acceptance is unavailable. Template construction requires a separately provisioned dedicated fast-sandbox runtime, template builder with kernel, artifact storage and KVM support. Run its complete owned create/Succeeded/instantiate/delete scenario there:

```bash
export OPENSANDBOX_RUST_TEMPLATE_PUBLISH=your-dedicated-artifact-prefix
cargo test --manifest-path tests/rust/Cargo.toml --locked --test kubernetes \
  k8s_template_create_ready_instantiate_delete -- --ignored --nocapture
```

All live scenarios are marked ignored so ordinary `cargo test` never allocates resources. Scripts explicitly execute their applicable scenarios. Legacy source-only tests in `e2e.rs` remain available, but do not replace the owned creation workflows.
