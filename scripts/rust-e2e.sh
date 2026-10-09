#!/usr/bin/env bash
# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0

# Use OPENSANDBOX_TEST_DOMAIN to test an existing server. Otherwise start an
# isolated Docker-backed server without changing ~/.sandbox.toml.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [[ -n "${OPENSANDBOX_TEST_DOMAIN:-}" ]]; then
  cargo test --manifest-path tests/rust/Cargo.toml --locked sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture
  exit
fi

SCRATCH_ROOT="${JCODE_SCRATCH_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
mkdir -p "$SCRATCH_ROOT"
RUN_DIR="$(mktemp -d "$SCRATCH_ROOT/opensandbox-rust-e2e.XXXXXX")"
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  echo "Rust E2E artifacts: $RUN_DIR"
}
trap cleanup EXIT

export OPENSANDBOX_TEST_PORT="${OPENSANDBOX_TEST_PORT:-18088}"
export OPENSANDBOX_TEST_DOMAIN="127.0.0.1:${OPENSANDBOX_TEST_PORT}"
export OPENSANDBOX_TEST_PROTOCOL=http
export OPENSANDBOX_TEST_NETWORK_POLICY="${OPENSANDBOX_TEST_NETWORK_POLICY:-true}"
export OPENSANDBOX_TEST_API_KEY="${OPENSANDBOX_TEST_API_KEY:-rust-e2e}"
export OPENSANDBOX_SANDBOX_DEFAULT_IMAGE="${OPENSANDBOX_SANDBOX_DEFAULT_IMAGE:-alpine:3.21}"
EXECD_IMAGE="${OPENSANDBOX_RUST_EXECD_IMAGE:-opensandbox/execd:rust-e2e}"
EGRESS_IMAGE="${OPENSANDBOX_RUST_EGRESS_IMAGE:-opensandbox/egress:rust-e2e}"

if [[ -z "${DOCKER_HOST:-}" ]]; then
  export DOCKER_HOST="$(docker context inspect --format '{{.Endpoints.docker.Host}}')"
fi

if [[ "${OPENSANDBOX_RUST_SKIP_IMAGE_BUILD:-0}" != 1 ]]; then
  docker build -f components/execd/Dockerfile -t "$EXECD_IMAGE" .
  docker build -f components/egress/Dockerfile -t "$EGRESS_IMAGE" .
fi
docker image inspect "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE" >/dev/null 2>&1 || docker pull "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE"

export SANDBOX_CONFIG_PATH="$RUN_DIR/server.toml"
# JSON strings are TOML-compatible. Quote caller input without shell expansion
# in configuration values or writing secrets to logs.
python3 - "$SANDBOX_CONFIG_PATH" <<'PY'
import json
import os
import sys
q = json.dumps
with open(sys.argv[1], "w") as f:
    f.write(f'''[server]
host = "127.0.0.1"
port = {int(os.environ["OPENSANDBOX_TEST_PORT"])}
api_key = {q(os.environ["OPENSANDBOX_TEST_API_KEY"])}
[log]
level = "INFO"
[runtime]
type = "docker"
execd_image = {q(os.environ.get("OPENSANDBOX_RUST_EXECD_IMAGE", "opensandbox/execd:rust-e2e"))}
[egress]
image = {q(os.environ.get("OPENSANDBOX_RUST_EGRESS_IMAGE", "opensandbox/egress:rust-e2e"))}
mode = "dns"
[docker]
network_mode = "bridge"
''')
PY
chmod 600 "$SANDBOX_CONFIG_PATH"
(
  cd server
  exec uv run --frozen python -m opensandbox_server.cli
) >"$RUN_DIR/server.log" 2>&1 &
SERVER_PID=$!

for attempt in {1..60}; do
  if curl --fail --silent "http://$OPENSANDBOX_TEST_DOMAIN/health" >/dev/null; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null || [[ "$attempt" == 60 ]]; then
    cat "$RUN_DIR/server.log"
    echo "Rust E2E server did not become healthy" >&2
    exit 1
  fi
  sleep 1
done
cargo test --manifest-path tests/rust/Cargo.toml --locked sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture 2>&1 | tee "$RUN_DIR/test-output.txt"
