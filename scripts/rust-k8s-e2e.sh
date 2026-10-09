#!/usr/bin/env bash
# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
# Never targets a caller's cluster. All config and logs live in external scratch.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$REPO_ROOT/scripts/common/kubernetes-e2e.sh"
for tool in kind docker kubectl helm cargo; do command -v "$tool" >/dev/null || { echo "Missing $tool. Install in external scratch, not into the repository." >&2; exit 1; }; done
SCRATCH_ROOT="${JCODE_SCRATCH_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
mkdir -p "$SCRATCH_ROOT"
RUN_DIR="$(mktemp -d "$SCRATCH_ROOT/rust-k8s-e2e.XXXXXX")"
chmod 700 "$RUN_DIR"
export OPENSANDBOX_RUST_RUN_ID="$(basename "$RUN_DIR" | tr '[:upper:].' '[:lower:]-')"
KIND_CLUSTER="$OPENSANDBOX_RUST_RUN_ID"
export KUBECONFIG="$RUN_DIR/kubeconfig"
KUBECONFIG_PATH="$KUBECONFIG"
E2E_NAMESPACE=rust-e2e
SERVER_NAMESPACE=opensandbox-system
SERVER_RELEASE=opensandbox-server
SERVER_VALUES_FILE="$RUN_DIR/server-values.yaml"
SERVER_IMG="opensandbox/server:$KIND_CLUSTER"
EXECD_IMG="opensandbox/execd:$KIND_CLUSTER"
EGRESS_IMG="opensandbox/egress:$KIND_CLUSTER"
INGRESS_IMG="opensandbox/ingress:$KIND_CLUSTER"
CONTROLLER_IMG="opensandbox/controller:$KIND_CLUSTER"
COMMITTER_IMG="opensandbox/image-committer:$KIND_CLUSTER"
SANDBOX_TEST_IMAGE="${OPENSANDBOX_SANDBOX_DEFAULT_IMAGE:-ubuntu:24.04}"
E2E_SERVER_GATEWAY_ENABLED=true
E2E_GATEWAY_ROUTE_MODE=uri
LIFECYCLE_LOCAL_PORT="${OPENSANDBOX_TEST_PORT:-18089}"
GATEWAY_LOCAL_PORT="${OPENSANDBOX_GATEWAY_PORT:-18090}"
INGRESS_GATEWAY_ADDRESS="127.0.0.1:$GATEWAY_LOCAL_PORT"
SERVER_IMG_REPOSITORY="${SERVER_IMG%:*}"; SERVER_IMG_TAG="${SERVER_IMG##*:}"
INGRESS_IMG_REPOSITORY="${INGRESS_IMG%:*}"; INGRESS_IMG_TAG="${INGRESS_IMG##*:}"
CREATED=0
PIDS=()
cleanup() {
  set +e
  for pid in "${PIDS[@]:-}"; do [[ -z "$pid" ]] || kill "$pid" 2>/dev/null || true; done
  if [[ "$CREATED" == 1 ]]; then
    kubectl get pods,jobs,sandboxsnapshots -A -o wide >"$RUN_DIR/resources.txt" 2>&1 || true
    kind export logs --name "$KIND_CLUSTER" "$RUN_DIR/kind-logs" >/dev/null 2>&1 || true
    kind delete cluster --name "$KIND_CLUSTER" >/dev/null
  fi
  echo "Rust Kubernetes E2E artifacts: $RUN_DIR"
}
trap cleanup EXIT INT TERM
cat >"$RUN_DIR/kind.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
containerdConfigPatches:
- |-
  [plugins."io.containerd.grpc.v1.cri".registry]
    config_path = "/etc/containerd/certs.d"
EOF
# Unique name means cleanup cannot delete a user's pre-existing cluster.
if kind get clusters | grep -Fx "$KIND_CLUSTER" >/dev/null; then echo "Refusing existing cluster" >&2; exit 1; fi
CREATED=1
kind create cluster --name "$KIND_CLUSTER" --kubeconfig "$KUBECONFIG" --config "$RUN_DIR/kind.yaml" --image "kindest/node:${KIND_K8S_VERSION:-v1.30.4}" --wait 180s
# Use only Helm and Docker builds. Repository Make targets can rewrite tracked Kustomize output.
docker build -t "$CONTROLLER_IMG" "$REPO_ROOT/kubernetes"
docker build -f "$REPO_ROOT/kubernetes/Dockerfile.image-committer" -t "$COMMITTER_IMG" "$REPO_ROOT/kubernetes"
k8s_e2e_build_runtime_images
for image in "$CONTROLLER_IMG" "$COMMITTER_IMG"; do kind load docker-image --name "$KIND_CLUSTER" "$image"; done
k8s_e2e_kind_load_runtime_images
helm install rust-base "$REPO_ROOT/manifests/charts/base" -n "$SERVER_NAMESPACE" --create-namespace
kubectl create namespace "$E2E_NAMESPACE"
# A registry owned by this cluster only. NodePort is reachable by both commit jobs and node pulls.
kubectl create deployment rust-registry --image=registry:2.8.3 -n "$E2E_NAMESPACE"
kubectl expose deployment rust-registry -n "$E2E_NAMESPACE" --port=5000 --type=NodePort
kubectl rollout status deployment/rust-registry -n "$E2E_NAMESPACE" --timeout=180s
NODE_IP="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}')"
REGISTRY_PORT="$(kubectl get svc rust-registry -n "$E2E_NAMESPACE" -o jsonpath='{.spec.ports[0].nodePort}')"
REGISTRY="$NODE_IP:$REGISTRY_PORT"
docker exec "$KIND_CLUSTER-control-plane" mkdir -p "/etc/containerd/certs.d/$REGISTRY"
printf 'server = "http://%s"\n[host."http://%s"]\n  capabilities = ["pull", "resolve", "push"]\n' "$REGISTRY" "$REGISTRY" | docker exec -i "$KIND_CLUSTER-control-plane" sh -c "cat > /etc/containerd/certs.d/$REGISTRY/hosts.toml"
helm install rust-controller "$REPO_ROOT/manifests/charts/controller" -n "$SERVER_NAMESPACE" \
  --set controller.image.repository="${CONTROLLER_IMG%:*}" --set controller.image.tag="${CONTROLLER_IMG##*:}" \
  --set controller.snapshot.imageCommitterImage="$COMMITTER_IMG" --set controller.snapshot.registry="$REGISTRY/rust-snapshots" \
  --set controller.snapshot.registryInsecure=true --set controller.snapshot.containerdSocketPath=/run/containerd/containerd.sock
k8s_e2e_write_server_helm_values
# The shared helper references a template absent from the chart. Provide an owned
# minimal BatchSandbox template explicitly rather than changing common helpers.
cat >"$RUN_DIR/batchsandbox.yaml" <<EOF
apiVersion: sandbox.opensandbox.io/v1alpha1
kind: BatchSandbox
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: sandbox
        image: ubuntu:24.04
EOF
kubectl create configmap rust-batchsandbox-template -n "$SERVER_NAMESPACE" --from-file=e2e.batchsandbox-template.yaml="$RUN_DIR/batchsandbox.yaml"
cat >"$RUN_DIR/template-values.yaml" <<EOF
server:
  image:
    repository: $SERVER_IMG_REPOSITORY
    tag: "$SERVER_IMG_TAG"
  gateway:
    enabled: true
    host: "$INGRESS_GATEWAY_ADDRESS"
    gatewayRouteMode: "$E2E_GATEWAY_ROUTE_MODE"
  volumes:
  - name: rust-template
    configMap:
      name: rust-batchsandbox-template
  volumeMounts:
  - name: rust-template
    mountPath: /etc/opensandbox/e2e.batchsandbox-template.yaml
    subPath: e2e.batchsandbox-template.yaml
EOF
chmod 600 "$SERVER_VALUES_FILE" "${SERVER_VALUES_FILE%.yaml}-ingress-gateway.yaml"
k8s_e2e_validate_rendered_config_toml
helm install "$SERVER_RELEASE" "$REPO_ROOT/manifests/charts/server" -n "$SERVER_NAMESPACE" -f "$SERVER_VALUES_FILE" -f "$RUN_DIR/template-values.yaml"
helm install opensandbox-ingress-gateway "$REPO_ROOT/manifests/charts/ingress-gateway" -n "$SERVER_NAMESPACE" -f "${SERVER_VALUES_FILE%.yaml}-ingress-gateway.yaml"
kubectl wait --for=condition=available --timeout=180s deployment/opensandbox-server deployment/opensandbox-ingress-gateway -n "$SERVER_NAMESPACE"
kubectl port-forward -n "$SERVER_NAMESPACE" svc/opensandbox-server "$LIFECYCLE_LOCAL_PORT:80" >"$RUN_DIR/server-forward.log" 2>&1 & PIDS+=("$!")
kubectl port-forward -n "$SERVER_NAMESPACE" svc/opensandbox-ingress-gateway "$GATEWAY_LOCAL_PORT:80" >"$RUN_DIR/gateway-forward.log" 2>&1 & PIDS+=("$!")
k8s_e2e_wait_http_ok "http://127.0.0.1:$LIFECYCLE_LOCAL_PORT/health"
k8s_e2e_wait_http_ok "http://127.0.0.1:$GATEWAY_LOCAL_PORT/status.ok"
export OPENSANDBOX_TEST_DOMAIN="127.0.0.1:$LIFECYCLE_LOCAL_PORT"
export OPENSANDBOX_SANDBOX_DEFAULT_IMAGE="$SANDBOX_TEST_IMAGE"
cargo test --manifest-path "$REPO_ROOT/tests/rust/Cargo.toml" --locked --test kubernetes k8s_secure_signed_pause_snapshot_restore -- --ignored --nocapture 2>&1 | tee "$RUN_DIR/tests.txt"
# BatchSandbox supports rootfs snapshots, not FSB VM templates. Never pretend these ran.
echo "Template workflow not runnable on Kind BatchSandbox: requires fast-sandbox VM runtime, builder/kernel, artifact storage and /dev/kvm. Run k8s_template_create_ready_instantiate_delete against a separately provisioned dedicated FSB server." >&2
exit 2
