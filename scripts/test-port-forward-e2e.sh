#!/usr/bin/env bash
# test-port-forward-e2e.sh — End-to-end port forwarding smoke test.
#
# Verifies that `pelagos run -p HOST:CONTAINER` works end-to-end:
#   1. VM is responsive
#   2. nginx:alpine container starts with -p 8080:80
#   3. curl http://localhost:8080/ returns an nginx response
#   4. Port is released after pelagos stop (curl fails)
#
# Depends on: a running VM (or cold-boots one), nginx:alpine image cached.
# Run after `bash scripts/build-vm-image.sh` and `bash scripts/sign.sh`.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
PELAGOS="$BINARY --kernel $KERNEL --initrd $INITRD --disk $DISK"

HOST_PORT=18090
CONT_PORT=80
CONTAINER_NAME="pf-e2e-$$"
IMG="public.ecr.aws/docker/library/nginx:alpine"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

cleanup() {
    $PELAGOS stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
    $PELAGOS rm   "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== port-forward e2e test ==="

# 1. VM responsive
printf "  ping... "
if ! $PELAGOS ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 2. Start nginx:alpine with port forward
printf "  pelagos run -d -p %d:%d nginx:alpine... " "$HOST_PORT" "$CONT_PORT"
RUN_OUT=$($PELAGOS run -d --name "$CONTAINER_NAME" \
    -p "${HOST_PORT}:${CONT_PORT}" "$IMG" 2>&1)
RUN_RC=$?
if [ "$RUN_RC" -eq 0 ]; then
    pass "pelagos run -d -p ${HOST_PORT}:${CONT_PORT} nginx:alpine (exit 0)"
else
    fail "pelagos run -d -p ${HOST_PORT}:${CONT_PORT} nginx:alpine (exit=$RUN_RC)"
    echo "    output: $RUN_OUT"
    exit 1
fi

# Give nginx and the port-dispatcher a moment to start.
sleep 3

# 3. curl reaches nginx through the port forward
CURL_OUT=$(curl -sf --max-time 5 "http://localhost:${HOST_PORT}/" 2>&1)
CURL_RC=$?
if [ "$CURL_RC" -eq 0 ] && echo "$CURL_OUT" | grep -qi "nginx\|Welcome"; then
    pass "curl http://localhost:${HOST_PORT}/ returns nginx response"
else
    fail "curl http://localhost:${HOST_PORT}/ (exit=$CURL_RC)"
    echo "    output: $CURL_OUT"
fi

# 4. Stop the container; port should be released
$PELAGOS stop "$CONTAINER_NAME" >/dev/null 2>&1
sleep 1
CURL2_OUT=$(curl -sf --max-time 3 "http://localhost:${HOST_PORT}/" 2>&1)
CURL2_RC=$?
if [ "$CURL2_RC" -ne 0 ]; then
    pass "curl after stop fails (port-dispatcher cleaned up)"
else
    fail "curl after stop should fail — port-dispatcher still listening"
fi

$PELAGOS rm "$CONTAINER_NAME" >/dev/null 2>&1 || true

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  port-forward e2e complete ($PASS passed)"
    exit 0
else
    echo "FAIL  $FAIL test(s) failed, $PASS passed"
    exit 1
fi
