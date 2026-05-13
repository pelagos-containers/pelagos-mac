#!/usr/bin/env bash
# test-pasta-smoke.sh -- Pasta networking smoke tests.
#
# Verifies:
#   1. Container can reach the internet via pasta (outbound connectivity)
#   2. Container with --port gets pasta listening on the VM side
#   3. nginx is reachable via the VM IP through pasta port forwarding
#   4. Container gets IPv6 address via pasta (GUA from VM's SLAAC)
#
# Note: localhost proxy (curl localhost:PORT) has a known bug (#281) where
# the Mac-side proxy connects to container_port instead of host_port.
# Test 3 validates pasta works by hitting the VM IP directly.
#
# Depends on: a running VM with pasta available.
# Run after a full rebuild (scripts/full-rebuild.sh).
set -uo pipefail

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

CONTAINER_NAME="pt-$$"
SANITY_NAME="ps-$$"
HOST_PORT=18091
CONT_PORT=80
VM_IP="192.168.105.2"

cleanup() {
    pelagos stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
    pelagos rm "$CONTAINER_NAME" >/dev/null 2>&1 || true
    pelagos stop "$SANITY_NAME" >/dev/null 2>&1 || true
    pelagos rm "$SANITY_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== pasta smoke test ==="

# 0. VM responsive
printf "  ping... "
if ! pelagos ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 1. Outbound internet connectivity (pasta default, no --port)
printf "  outbound internet... "
RUN_OUT=$(pelagos run -d --name "$SANITY_NAME" alpine wget -qO- http://ifconfig.me/ip 2>&1)
RUN_RC=$?
if [ "$RUN_RC" -ne 0 ]; then
    fail "container start (exit=$RUN_RC)"
    echo "    output: $RUN_OUT"
else
    sleep 8
    LOGS=$(pelagos logs "$SANITY_NAME" 2>&1)
    if echo "$LOGS" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+'; then
        IP=$(echo "$LOGS" | grep -oE '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | head -1)
        pass "outbound works (external IP: $IP)"
    else
        fail "outbound connectivity"
        echo "    output: $LOGS"
    fi
fi
pelagos stop "$SANITY_NAME" >/dev/null 2>&1 || true
pelagos rm "$SANITY_NAME" >/dev/null 2>&1 || true

# 2. Start nginx with port forwarding
printf "  pelagos run -d --port %d:%d nginx... " "$HOST_PORT" "$CONT_PORT"
RUN_OUT=$(pelagos run -d --name "$CONTAINER_NAME" \
    --port "${HOST_PORT}:${CONT_PORT}" nginx 2>&1)
RUN_RC=$?
if [ "$RUN_RC" -eq 0 ]; then
    pass "nginx started with port forward"
else
    fail "nginx start (exit=$RUN_RC)"
    echo "    output: $RUN_OUT"
    exit 1
fi

sleep 5

# 3. pasta is listening on the VM
printf "  pasta listening on VM:%d... " "$HOST_PORT"
SS_OUT=$(pelagos vm ssh -- "netstat -tlnp 2>/dev/null | grep $HOST_PORT" 2>&1)
if echo "$SS_OUT" | grep -q "$HOST_PORT"; then
    pass "pasta listening on $HOST_PORT"
else
    fail "pasta not listening on $HOST_PORT"
    echo "    output: $SS_OUT"
fi

# 4. nginx reachable via VM IP (direct, bypasses Mac proxy)
printf "  curl http://%s:%d/... " "$VM_IP" "$HOST_PORT"
CURL_OUT=$(curl -sf --max-time 5 "http://${VM_IP}:${HOST_PORT}/" 2>&1)
CURL_RC=$?
if [ "$CURL_RC" -eq 0 ] && echo "$CURL_OUT" | grep -qi "nginx\|Welcome"; then
    pass "nginx reachable via VM IP"
else
    fail "curl to VM IP (exit=$CURL_RC)"
    echo "    output: $CURL_OUT"
fi

# 5. Container has IPv6 GUA via pasta
printf "  container IPv6 (GUA via pasta)... "
IPV6_OUT=$(pelagos exec "$CONTAINER_NAME" -- /bin/sh -c "cat /proc/net/if_inet6 2>/dev/null" 2>&1)
if echo "$IPV6_OUT" | grep -q "eth0" && echo "$IPV6_OUT" | grep -v "^fe80" | grep -v "^0000" | grep -q "eth0"; then
    GUA_LINE=$(echo "$IPV6_OUT" | grep "eth0" | grep -v "^fe80" | grep -v "^0000" | head -1)
    pass "container has GUA on eth0"
else
    # IPv6 may not be available on all test networks
    echo "SKIP (no GUA found -- host network may lack IPv6)"
fi

# 6. Port released after stop
printf "  port released after stop... "
pelagos stop "$CONTAINER_NAME" >/dev/null 2>&1
sleep 2
SS_AFTER=$(pelagos vm ssh -- "netstat -tlnp 2>/dev/null | grep $HOST_PORT" 2>&1)
if echo "$SS_AFTER" | grep -q "$HOST_PORT"; then
    fail "pasta still listening after stop"
else
    pass "port released"
fi
pelagos rm "$CONTAINER_NAME" >/dev/null 2>&1 || true

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  pasta smoke test complete ($PASS passed)"
    exit 0
else
    echo "FAIL  $FAIL test(s) failed, $PASS passed"
    exit 1
fi
