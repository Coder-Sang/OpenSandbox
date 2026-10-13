#!/usr/bin/env bash
# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0

# Dedicated plaintext and TLS-only fixtures. Never use an existing Redis,
# FLUSHDB, the host trust store, or a shared certificate directory.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
umask 077
SCRATCH_ROOT="${JCODE_SCRATCH_DIR:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}"
mkdir -p "$SCRATCH_ROOT"
RUN_DIR="$(mktemp -d "$SCRATCH_ROOT/opensandbox-rust-redis.XXXXXX")"
RUN_ID="$(basename "$RUN_DIR")"
PLAIN_CONTAINER="$RUN_ID-plain"
TLS_CONTAINER="$RUN_ID-tls"
CERT_DIR="$RUN_DIR/certs"
mkdir -m 700 "$CERT_DIR"
cleanup() {
  set +e
  for container in "$PLAIN_CONTAINER" "$TLS_CONTAINER"; do
    docker logs "$container" >"$RUN_DIR/$container.log" 2>&1
    docker rm -f "$container" >/dev/null 2>&1
  done
  echo "Rust Redis E2E artifacts: $RUN_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
IMAGE="${OPENSANDBOX_RUST_REDIS_IMAGE:-redis:7-alpine}"
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull "$IMAGE"
docker image inspect --format '{{.Id}}' "$IMAGE" >"$RUN_DIR/image.txt"

openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
  -subj /CN=opensandbox-rust-test-ca -keyout "$CERT_DIR/ca.key" \
  -out "$CERT_DIR/ca.crt" >"$RUN_DIR/certificates.log" 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 2 \
  -subj /CN=opensandbox-rust-unrelated-ca -keyout "$CERT_DIR/unrelated.key" \
  -out "$CERT_DIR/unrelated.crt" >>"$RUN_DIR/certificates.log" 2>&1
openssl req -new -newkey rsa:2048 -nodes -sha256 -subj /CN=localhost \
  -keyout "$CERT_DIR/server.key" -out "$CERT_DIR/server.csr" >>"$RUN_DIR/certificates.log" 2>&1
cat >"$CERT_DIR/server.ext" <<'EOF'
subjectAltName=DNS:localhost
extendedKeyUsage=serverAuth
keyUsage=digitalSignature,keyEncipherment
basicConstraints=CA:FALSE
EOF
openssl x509 -req -in "$CERT_DIR/server.csr" -CA "$CERT_DIR/ca.crt" \
  -CAkey "$CERT_DIR/ca.key" -CAcreateserial -days 2 -sha256 \
  -extfile "$CERT_DIR/server.ext" -out "$CERT_DIR/server.crt" >>"$RUN_DIR/certificates.log" 2>&1
# Synthetic, fixture-only AUTH. Configuration and evidence remain private.
PASSWORD=opensandbox-rust-redis-fixture
cat >"$CERT_DIR/redis.conf" <<EOF
port 0
tls-port 6379
tls-cert-file /certs/server.crt
tls-key-file /certs/server.key
tls-ca-cert-file /certs/ca.crt
tls-auth-clients no
requirepass $PASSWORD
save ""
appendonly no
EOF
chmod 600 "$CERT_DIR"/*
docker run -d --name "$PLAIN_CONTAINER" --label "rust-redis-run=$RUN_ID" \
  --user 0:0 --cap-drop ALL --security-opt no-new-privileges --entrypoint redis-server \
  -p 127.0.0.1::6379 "$IMAGE" --save '' --appendonly no >/dev/null
# Root only inside this dedicated container allows reading the private read-only
# key mount; no host ownership, mode or trust configuration is changed.
docker run -d --name "$TLS_CONTAINER" --label "rust-redis-run=$RUN_ID" \
  --user 0:0 --cap-drop ALL --security-opt no-new-privileges --entrypoint redis-server \
  -p 127.0.0.1::6379 -v "$CERT_DIR:/certs:ro" "$IMAGE" \
  /certs/redis.conf >/dev/null
PLAIN_PORT="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "6379/tcp") 0).HostPort}}' "$PLAIN_CONTAINER")"
TLS_PORT="$(docker inspect --format '{{(index (index .NetworkSettings.Ports "6379/tcp") 0).HostPort}}' "$TLS_CONTAINER")"
READY=0
for attempt in {1..30}; do
  if [[ "$(docker exec "$PLAIN_CONTAINER" redis-cli ping 2>/dev/null)" == PONG ]] && \
     [[ "$(docker exec -e "REDISCLI_AUTH=$PASSWORD" "$TLS_CONTAINER" redis-cli \
       --tls --cacert /certs/ca.crt -h localhost ping 2>/dev/null)" == PONG ]]; then
    READY=1
    break
  fi
  sleep 1
done
[[ "$READY" == 1 ]] || { echo "Dedicated Redis fixtures failed readiness" >&2; exit 1; }
docker exec "$PLAIN_CONTAINER" redis-server --version >"$RUN_DIR/redis-version.txt"
export OPENSANDBOX_REDIS_TEST=1
export OPENSANDBOX_REDIS_TLS_TEST=1
export OPENSANDBOX_REDIS_TLS_URL="rediss://:$PASSWORD@localhost:$TLS_PORT/0"
export OPENSANDBOX_REDIS_TLS_WRONG_HOST_URL="rediss://:$PASSWORD@127.0.0.1:$TLS_PORT/0"
export OPENSANDBOX_REDIS_TLS_CA="$CERT_DIR/ca.crt"
export OPENSANDBOX_REDIS_TLS_UNTRUSTED_CA="$CERT_DIR/unrelated.crt"
read -r -a TOOLCHAINS <<<"${OPENSANDBOX_RUST_TOOLCHAINS:-1.85.1 stable}"
FAILED=0
for toolchain in "${TOOLCHAINS[@]}"; do
  OPENSANDBOX_REDIS_URL="redis://127.0.0.1:$PLAIN_PORT/0" \
    cargo "+$toolchain" test --manifest-path sdks/sandbox/rust/Cargo.toml --locked --features redis \
    --test redis_pool -- --nocapture 2>&1 | tee "$RUN_DIR/plain-$toolchain.txt" || FAILED=1
  # Trust changes apply only to this process and its children.
  env -u SSL_CERT_DIR SSL_CERT_FILE="$CERT_DIR/ca.crt" OPENSANDBOX_REDIS_URL="$OPENSANDBOX_REDIS_TLS_URL" \
    cargo "+$toolchain" test --manifest-path sdks/sandbox/rust/Cargo.toml --locked --features redis \
    --test redis_pool -- --nocapture 2>&1 | tee "$RUN_DIR/tls-pool-$toolchain.txt" || FAILED=1
  cargo "+$toolchain" test --manifest-path sdks/sandbox/rust/Cargo.toml --locked --features redis \
    --test redis_tls -- --nocapture 2>&1 | tee "$RUN_DIR/tls-validation-$toolchain.txt" || FAILED=1
done
exit "$FAILED"
