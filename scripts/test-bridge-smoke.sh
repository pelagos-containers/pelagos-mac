#!/usr/bin/env bash
# test-bridge-smoke.sh -- Bridge networking smoke tests.
#
# Verifies:
#   1. Bridge kernel modules (bridge, stp, llc) are loaded in the VM
#   2. Two containers on an explicit bridge network can ping each other (c2c)
#   3. Bridge network cleanup works (stop, rm, network rm)
#
# Depends on: a running VM with bridge modules staged in the initramfs.
# Run after a full rebuild (scripts/full-rebuild.sh).
set -uo pipefail

PASS=0
FAIL=0

pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

NETWORK_NAME="brtest$$"
S1="brs1-$$"
S2="brs2-$$"

cleanup() {
    pelagos stop "$S1" >/dev/null 2>&1 || true
    pelagos stop "$S2" >/dev/null 2>&1 || true
    pelagos rm "$S1" >/dev/null 2>&1 || true
    pelagos rm "$S2" >/dev/null 2>&1 || true
    pelagos network rm "$NETWORK_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== bridge smoke test ==="

# 0. VM responsive
printf "  ping... "
if ! pelagos ping 2>&1 | grep -q pong; then
    echo "FAIL (VM not responsive)"
    exit 1
fi
echo "ok"

# 1. Bridge modules loaded
printf "  bridge modules... "
MODS=$(pelagos vm ssh -- "cat /proc/modules" 2>&1)
if echo "$MODS" | grep -q "^bridge " \
    && echo "$MODS" | grep -q "^stp " \
    && echo "$MODS" | grep -q "^llc "; then
    pass "bridge, stp, llc all loaded"
else
    fail "missing bridge modules"
    echo "    loaded: $(echo "$MODS" | grep -E '^(bridge|stp|llc) ' || echo 'none')"
fi

# 2. Create a bridge network
printf "  create network %s... " "$NETWORK_NAME"
NET_OUT=$(pelagos network create "$NETWORK_NAME" --subnet 172.29.0.0/24 2>&1)
NET_RC=$?
if [ "$NET_RC" -eq 0 ]; then
    pass "network created"
else
    fail "network create (exit=$NET_RC)"
    echo "    output: $NET_OUT"
    exit 1
fi

# 3. Start two containers on the bridge
printf "  start %s on bridge... " "$S1"
S1_OUT=$(pelagos run -d --name "$S1" --network "$NETWORK_NAME" alpine sleep 120 2>&1)
S1_RC=$?
if [ "$S1_RC" -eq 0 ]; then
    pass "$S1 started"
else
    fail "$S1 start (exit=$S1_RC)"
    echo "    output: $S1_OUT"
    exit 1
fi

printf "  start %s on bridge... " "$S2"
S2_OUT=$(pelagos run -d --name "$S2" --network "$NETWORK_NAME" alpine sleep 120 2>&1)
S2_RC=$?
if [ "$S2_RC" -eq 0 ]; then
    pass "$S2 started"
else
    fail "$S2 start (exit=$S2_RC)"
    echo "    output: $S2_OUT"
    exit 1
fi

# 4. Get s1's IP and ping it from s2
# s1 should be 172.29.0.2 (first container), s2 should be 172.29.0.3
S1_IP="172.29.0.2"
printf "  c2c ping %s -> %s (%s)... " "$S2" "$S1" "$S1_IP"
# Run ping as a new container since exec + alpine ping can be tricky
PING_OUT=$(pelagos run -d --name "brp-$$" --network "$NETWORK_NAME" alpine ping -c3 -W3 "$S1_IP" 2>&1)
PING_RC=$?
if [ "$PING_RC" -ne 0 ]; then
    fail "ping container start (exit=$PING_RC)"
    echo "    output: $PING_OUT"
else
    sleep 5
    PING_LOGS=$(pelagos logs "brp-$$" 2>&1)
    if echo "$PING_LOGS" | grep -q "0% packet loss"; then
        pass "c2c ping: 0% loss"
    elif echo "$PING_LOGS" | grep -q "bytes from"; then
        pass "c2c ping: replies received"
    else
        fail "c2c ping"
        echo "    output: $PING_LOGS"
    fi
    pelagos stop "brp-$$" >/dev/null 2>&1 || true
    pelagos rm "brp-$$" >/dev/null 2>&1 || true
fi

# 5. Cleanup
printf "  cleanup... "
pelagos stop "$S1" >/dev/null 2>&1 || true
pelagos stop "$S2" >/dev/null 2>&1 || true
pelagos rm "$S1" >/dev/null 2>&1 || true
pelagos rm "$S2" >/dev/null 2>&1 || true
RM_OUT=$(pelagos network rm "$NETWORK_NAME" 2>&1)
RM_RC=$?
if [ "$RM_RC" -eq 0 ]; then
    pass "cleanup succeeded"
else
    fail "network rm (exit=$RM_RC)"
    echo "    output: $RM_OUT"
fi

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  bridge smoke test complete ($PASS passed)"
    exit 0
else
    echo "FAIL  $FAIL test(s) failed, $PASS passed"
    exit 1
fi
