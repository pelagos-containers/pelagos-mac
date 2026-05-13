#!/usr/bin/env bash
# test-port-forward-e2e.sh -- End-to-end port forwarding smoke test.
#
# Verifies that `pelagos run --port HOST:CONTAINER` works end-to-end:
#   1. VM is responsive
#   2. nginx container starts with --port 18090:80
#   3. curl http://localhost:18090/ returns an nginx response
#   4. Port is released after pelagos stop (curl fails)
#
# Depends on: a running VM, nginx image cached or pullable.
set -uo pipefail

HOST_PORT=18090
CONT_PORT=80
CONTAINER_NAME="pfe-$$"

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

cleanup() {
    pelagos stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
    pelagos rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== port-forward e2e test ==="

# 1. VM responsive
printf "  ping... "
if ! pelagos ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 2. Start nginx with port forward
printf "  pelagos run -d --port %d:%d nginx... " "$HOST_PORT" "$CONT_PORT"
RUN_OUT=$(pelagos run -d --name "$CONTAINER_NAME" \
    --port "${HOST_PORT}:${CONT_PORT}" nginx 2>&1)
RUN_RC=$?
if [ "$RUN_RC" -eq 0 ]; then
    pass "container started (exit 0)"
else
    fail "container start (exit=$RUN_RC)"
    echo "    output: $RUN_OUT"
    exit 1
fi

# 3. Wait for pasta + nginx, then curl localhost
CURL_RC=1
for i in $(seq 1 10); do
    sleep 2
    CURL_OUT=$(curl -sf --max-time 3 "http://localhost:${HOST_PORT}/" 2>&1)
    CURL_RC=$?
    if [ "$CURL_RC" -eq 0 ]; then break; fi
done

if [ "$CURL_RC" -eq 0 ] && echo "$CURL_OUT" | grep -qi "nginx\|Welcome"; then
    pass "curl http://localhost:${HOST_PORT}/ returns nginx response"
else
    fail "curl http://localhost:${HOST_PORT}/ (exit=$CURL_RC)"
    echo "    output: $CURL_OUT"
fi

# 4. Stop the container; port should be released
pelagos stop "$CONTAINER_NAME" >/dev/null 2>&1
sleep 2
CURL2_OUT=$(curl -sf --max-time 3 "http://localhost:${HOST_PORT}/" 2>&1)
CURL2_RC=$?
if [ "$CURL2_RC" -ne 0 ]; then
    pass "curl after stop fails (port released)"
else
    fail "curl after stop should fail -- port still listening"
fi

pelagos rm "$CONTAINER_NAME" >/dev/null 2>&1 || true

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  port-forward e2e complete ($PASS passed)"
    exit 0
else
    echo "FAIL  $FAIL test(s) failed, $PASS passed"
    exit 1
fi
