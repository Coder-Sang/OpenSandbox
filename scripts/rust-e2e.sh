#!/usr/bin/env bash
# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0

# Use OPENSANDBOX_TEST_DOMAIN to test an existing server. Otherwise start an
# isolated Docker-backed server without changing ~/.sandbox.toml.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# All evidence, including caller-configured runs, belongs to a fresh private
# invocation directory. Never reuse or chmod a caller's directory.
umask 077
SCRATCH_ROOT="${JCODE_SCRATCH_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
mkdir -p "$SCRATCH_ROOT"
RUN_DIR="$(mktemp -d "$SCRATCH_ROOT/opensandbox-rust-e2e.XXXXXX")"
export OPENSANDBOX_RUST_ARTIFACT_DIR="$RUN_DIR/interpreter"
mkdir -m 700 "$OPENSANDBOX_RUST_ARTIFACT_DIR"
if [[ "${OPENSANDBOX_RUST_INTERPRETER_ONLY:-false}" == true && "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" == true ]]; then
  echo "Select only one focused E2E suite" >&2
  exit 1
fi

run_scenarios() {
  local proxy
  local expanded_args=(--ignored --nocapture --test-threads=1)
  if [[ "${RUN_CODE_INTERPRETER_E2E:-false}" != true ]]; then
    expanded_args+=(--skip docker_interpreter)
  fi
  FAILED=0
  if [[ "${OPENSANDBOX_RUST_INTERPRETER_ONLY:-false}" == true ]]; then
    local selected="$RUN_DIR/interpreter-selection.txt"
    if ! cargo test --manifest-path tests/rust/Cargo.toml --locked --test expanded "${OPENSANDBOX_RUST_INTERPRETER_FILTER:-docker_interpreter}" -- --ignored --list >"$selected" 2>&1; then
      cat "$selected"
      FAILED=1
      return
    fi
    if ! grep -qE '(^|::)docker_interpreter[^ ]*: test$' "$selected"; then
      echo "Interpreter filter selected no live interpreter tests" >&2
      FAILED=1
      return
    fi
  fi
  if [[ "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" == true ]]; then
    local selected="$RUN_DIR/identity-argv-selection.txt"
    if ! cargo test --manifest-path tests/rust/Cargo.toml --locked --test identity_argv -- --ignored --list >"$selected" 2>&1; then
      cat "$selected"
      FAILED=1
      return
    fi
    if ! grep -q '^docker_identity_files_permissions_ownership_and_all_routes: test$' "$selected" ||
       ! grep -q '^docker_native_argv_literal_options_stream_background_failure_interrupt: test$' "$selected"; then
      echo "UID/GID or argv live scenarios are missing" >&2
      FAILED=1
      return
    fi
  fi
  for proxy in false true; do
    export OPENSANDBOX_TEST_USE_SERVER_PROXY="$proxy"
    if [[ "${OPENSANDBOX_RUST_INTERPRETER_ONLY:-false}" == true ]]; then
      cargo test --manifest-path tests/rust/Cargo.toml --locked --test expanded "${OPENSANDBOX_RUST_INTERPRETER_FILTER:-docker_interpreter}" -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/interpreter-$proxy.txt" || FAILED=1
    elif [[ "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" == true ]]; then
      cargo test --manifest-path tests/rust/Cargo.toml --locked --test identity_argv -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/identity-argv-$proxy.txt" || FAILED=1
    else
      if ! cargo test --manifest-path tests/rust/Cargo.toml --locked --test prerequisites docker_isolation_runtime_prerequisites -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/prerequisites-$proxy.txt"; then
        FAILED=1
        continue
      fi
      cargo test --manifest-path tests/rust/Cargo.toml --locked sandbox_create_command_files_metrics_egress_kill -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/basic-$proxy.txt" || FAILED=1
      cargo test --manifest-path tests/rust/Cargo.toml --locked --test expanded docker_ -- "${expanded_args[@]}" 2>&1 | tee "$RUN_DIR/expanded-$proxy.txt" || FAILED=1
      cargo test --manifest-path tests/rust/Cargo.toml --locked --test overlays docker_ -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/overlays-$proxy.txt" || FAILED=1
      cargo test --manifest-path tests/rust/Cargo.toml --locked --test identity_argv -- --ignored --nocapture --test-threads=1 2>&1 | tee "$RUN_DIR/identity-argv-$proxy.txt" || FAILED=1
    fi
  done
}

if [[ -n "${OPENSANDBOX_TEST_DOMAIN:-}" ]]; then
  : "${OPENSANDBOX_RUST_RUN_ID:?Set a unique run ID for existing-server E2E ownership}"
  trap 'echo "Rust E2E artifacts: $RUN_DIR"' EXIT
  run_scenarios
  exit "$FAILED"
fi

SERVER_PID=""
RUN_FINISHED=false
export OPENSANDBOX_RUST_RUN_ID="$(basename "$RUN_DIR")"
TARGET_CONTAINER="$OPENSANDBOX_RUST_RUN_ID-vault-target"
cleanup() {
  local exit_status=$?
  set +e
  docker ps -aq --filter "label=rust-e2e-run=$OPENSANDBOX_RUST_RUN_ID" 2>/dev/null | while read -r id; do
    [[ -z "$id" ]] || docker logs "$id" >"$RUN_DIR/container-$id.log" 2>&1
  done
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  docker logs "$TARGET_CONTAINER" >"$RUN_DIR/vault-target.log" 2>&1 || true
  docker rm -f "$TARGET_CONTAINER" >/dev/null 2>&1 || true
  # Only this invocation's labels qualify. Never clean broad e2e labels.
  docker ps -aq --filter "label=rust-e2e-run=$OPENSANDBOX_RUST_RUN_ID" 2>/dev/null | while read -r id; do
    [[ -z "$id" ]] || docker rm -f "$id" >/dev/null
  done
  echo "Rust E2E artifacts: $RUN_DIR"
  # Bash 3.2 can report zero to EXIT after a fatal nounset expansion. A run
  # that never reached the scenario result must still fail.
  if [[ "$exit_status" == 0 && "$RUN_FINISHED" != true ]]; then
    exit_status=1
  fi
  exit "$exit_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export OPENSANDBOX_TEST_PORT="${OPENSANDBOX_TEST_PORT:-18088}"
export OPENSANDBOX_TEST_DOMAIN="127.0.0.1:${OPENSANDBOX_TEST_PORT}"
export OPENSANDBOX_TEST_PROTOCOL=http
export OPENSANDBOX_TEST_NETWORK_POLICY="${OPENSANDBOX_TEST_NETWORK_POLICY:-true}"
export OPENSANDBOX_TEST_API_KEY="${OPENSANDBOX_TEST_API_KEY:-rust-e2e}"
if [[ -z "${OPENSANDBOX_SANDBOX_DEFAULT_IMAGE:-}" ]]; then
  if [[ "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" == true ]]; then
    export OPENSANDBOX_SANDBOX_DEFAULT_IMAGE=python:3.11-slim-bookworm
  else
    export OPENSANDBOX_SANDBOX_DEFAULT_IMAGE="opensandbox/rust-e2e-runtime:$OPENSANDBOX_RUST_RUN_ID"
    docker build -t "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE" tests/rust/fixtures
    export OPENSANDBOX_RUST_DEDICATED_JUPYTER=true
  fi
fi
EXECD_IMAGE="${OPENSANDBOX_RUST_EXECD_IMAGE:-opensandbox/execd:$OPENSANDBOX_RUST_RUN_ID}"
EGRESS_IMAGE="${OPENSANDBOX_RUST_EGRESS_IMAGE:-opensandbox/egress:$OPENSANDBOX_RUST_RUN_ID}"

if [[ -z "${DOCKER_HOST:-}" ]]; then
  export DOCKER_HOST="$(docker context inspect --format '{{.Endpoints.docker.Host}}')"
fi
if [[ "$(docker info --format '{{.OSType}}')" != linux ]]; then
  echo "Rust Docker E2E requires a Linux Docker daemon with namespace and overlayfs support" >&2
  exit 1
fi

if [[ "${OPENSANDBOX_RUST_SKIP_IMAGE_BUILD:-0}" != 1 ]]; then
  docker build -f components/execd/Dockerfile -t "$EXECD_IMAGE" .
  if [[ "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" != true ]]; then
    docker build -f components/egress/Dockerfile -t "$EGRESS_IMAGE" .
  fi
fi
docker image inspect "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE" >/dev/null 2>&1 || docker pull "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE"
{
  rustc --version
  docker info --format 'Docker kernel={{.KernelVersion}} architecture={{.Architecture}}'
  docker image inspect --format '{{.Id}}' "$EXECD_IMAGE" "$OPENSANDBOX_SANDBOX_DEFAULT_IMAGE"
} >"$RUN_DIR/runtime.txt"

export OPENSANDBOX_RUST_EXECD_IMAGE="$EXECD_IMAGE" OPENSANDBOX_RUST_EGRESS_IMAGE="$EGRESS_IMAGE"
if [[ "${OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY:-false}" != true ]]; then
  docker run -d --name "$TARGET_CONTAINER" --label "rust-e2e-run=$OPENSANDBOX_RUST_RUN_ID" \
    -v "$REPO_ROOT/tests/rust/fixtures:/srv:ro" python:3.11-alpine python /srv/vault_target.py >/dev/null
  export OPENSANDBOX_RUST_VAULT_TARGET_IP="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$TARGET_CONTAINER")"
  TARGET_READY=0
  for attempt in {1..30}; do
    if docker exec "$TARGET_CONTAINER" python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1/healthz', timeout=1).read()" >/dev/null 2>&1; then TARGET_READY=1; break; fi
    sleep 1
  done
  [[ "$TARGET_READY" == 1 ]] || { echo "Dedicated vault target failed readiness" >&2; exit 1; }
fi
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
mode = "dns+nft"
[docker]
network_mode = "bridge"
[proxy]
# The server runs on the host, not inside Docker's bridge network.
# Use published endpoints, including on macOS where bridge IPs are unreachable.
resolve_internal = false
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
run_scenarios
RUN_FINISHED=true
exit "$FAILED"
