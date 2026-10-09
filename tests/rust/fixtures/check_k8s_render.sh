#!/usr/bin/env bash
# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
source "$REPO_ROOT/scripts/common/kubernetes-e2e.sh"
RUN_DIR="$(mktemp -d "${JCODE_SCRATCH_DIR:-${TMPDIR:-/tmp}}/rust-helm-check.XXXXXX")"
chmod 700 "$RUN_DIR"
SERVER_VALUES_FILE="$RUN_DIR/server-values.yaml"
SERVER_IMG_REPOSITORY=opensandbox/server
SERVER_IMG_TAG=render-check
EXECD_IMG=opensandbox/execd:render-check
EGRESS_IMG=opensandbox/egress:render-check
INGRESS_IMG_REPOSITORY=opensandbox/ingress
INGRESS_IMG_TAG=render-check
E2E_NAMESPACE=rust-e2e
E2E_SERVER_GATEWAY_ENABLED=true
E2E_GATEWAY_ROUTE_MODE=uri
INGRESS_GATEWAY_ADDRESS=127.0.0.1:18090
k8s_e2e_write_server_helm_values
chmod 600 "$SERVER_VALUES_FILE" "$GATEWAY_VALUES_FILE"
# Extract the runner's exact mount overlay without executing any runtime commands.
awk '/cat >"\$RUN_DIR\/template-values.yaml" <<EOF/{capture=1;next} capture && /^EOF$/{exit} capture{print}' "$REPO_ROOT/scripts/rust-k8s-e2e.sh" >"$RUN_DIR/template-values.yaml"
# The overlay references variables. Feed the known check values to the shell heredoc only.
# Use Helm --set for image/address overrides so literal extracted variables cannot hide drift.
helm template rust-controller "$REPO_ROOT/manifests/charts/controller" --set controller.image.repository=opensandbox/controller --set controller.image.tag=render-check --set controller.snapshot.imageCommitterImage=opensandbox/image-committer:render-check --set controller.snapshot.registry=127.0.0.1:30500/rust-snapshots --set controller.snapshot.registryInsecure=true --set controller.snapshot.containerdSocketPath=/run/containerd/containerd.sock >"$RUN_DIR/controller.yaml"
helm template opensandbox-server "$REPO_ROOT/manifests/charts/server" -f "$SERVER_VALUES_FILE" -f "$RUN_DIR/template-values.yaml" --set server.image.repository=opensandbox/server --set server.image.tag=render-check --set server.gateway.host=127.0.0.1:18090 --set server.gateway.gatewayRouteMode=uri >"$RUN_DIR/server.yaml"
helm template opensandbox-ingress-gateway "$REPO_ROOT/manifests/charts/ingress-gateway" -f "$GATEWAY_VALUES_FILE" >"$RUN_DIR/gateway.yaml"
"$REPO_ROOT/server/.venv/bin/python" "$REPO_ROOT/tests/rust/fixtures/validate_k8s_render.py" "$RUN_DIR"
echo "Render artifacts: $RUN_DIR"
