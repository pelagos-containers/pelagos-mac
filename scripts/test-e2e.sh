#!/usr/bin/env bash
# test-e2e.sh — End-to-end integration tests for pelagos-mac.
#
# Usage:
#   ./test-e2e.sh             — functional tests (auto-starts daemon if needed)
#   ./test-e2e.sh --cold      — stop daemon first, then run functional + daemon lifecycle tests
#   ./test-e2e.sh --warm      — skip functional tests; only verify warm-reuse timing + lifecycle
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)
#
# If image pulls fail with "error sending request", PF state has degraded.
# Fix with: sudo pfctl -f /etc/pf.conf

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"

MODE="functional"
if [ "${1:-}" = "--cold" ]; then MODE="cold"; fi
if [ "${1:-}" = "--warm" ]; then MODE="warm"; fi

PASS=0
FAIL=0

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

pelagos() {
    "$BINARY" \
        --kernel  "$KERNEL" \
        --initrd  "$INITRD" \
        --disk    "$DISK" \
        --cmdline "$CMDLINE" \
        "$@" 2>&1
}

pass() { PASS=$((PASS + 1)); echo "  [PASS] $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  [FAIL] $1"; }

check_file() {
    if [ -f "$2" ]; then
        echo "  [OK]   $1: $2"
    else
        echo "  [FAIL] $1 missing: $2"
        FAIL=$((FAIL + 1))
    fi
}

# Milliseconds since epoch — portable via python3 (macOS date lacks %N).
ms_now() { python3 -c "import time; print(int(time.time() * 1000))"; }

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

echo "=== preflight (mode: $MODE) ==="
check_file "kernel"    "$KERNEL"
check_file "initramfs" "$INITRD"
check_file "disk"      "$DISK"
check_file "binary"    "$BINARY"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FAIL: preflight failed. Run 'make image' and 'make sign' first."
    exit 1
fi

# ---------------------------------------------------------------------------
# Cold-mode setup: stop any running daemon so test 1 measures a real cold start
# ---------------------------------------------------------------------------

if [ "$MODE" = "cold" ]; then
    echo ""
    echo "=== cold-mode setup: stopping any running daemon ==="
    pelagos vm stop > /dev/null 2>&1 || true
    sleep 1
    OUT=$(pelagos vm status 2>&1 || true)
    if echo "$OUT" | grep -q "stopped"; then
        echo "  [OK]   daemon stopped"
    else
        echo "  [WARN] vm status: $OUT (may already be stopped)"
    fi
fi

# ---------------------------------------------------------------------------
# Functional tests (skipped in --warm mode)
# ---------------------------------------------------------------------------

if [ "$MODE" != "warm" ]; then

# ---------------------------------------------------------------------------
# Test 1: ping (cold-start timing captured in cold mode)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 1: ping ==="
if [ "$MODE" = "cold" ]; then
    T0=$(ms_now); OUT=$(pelagos ping); T1=$(ms_now)
    COLD_MS=$(( T1 - T0 ))
    echo "  [TIME] cold-start ping: ${COLD_MS} ms"
else
    OUT=$(pelagos ping)
fi
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^pong$"; then
    pass "ping returned 'pong'"
else
    fail "ping did not return 'pong' (got: $(echo "$OUT" | grep -v '^\['))"
fi

# ---------------------------------------------------------------------------
# Test 2: echo hello
# ---------------------------------------------------------------------------

echo ""
echo "=== test 2: run alpine /bin/echo hello ==="
OUT=$(pelagos run alpine /bin/echo hello)
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^hello$"; then
    pass "output contains 'hello'"
else
    fail "expected 'hello', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 3: sh -c (hyphen arg passthrough)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 3: run alpine /bin/sh -c 'echo foo; echo bar' ==="
OUT=$(pelagos run alpine /bin/sh -c "echo foo; echo bar")
echo "$OUT" | grep -v "^\["
if echo "$OUT" | grep -q "^foo$" && echo "$OUT" | grep -q "^bar$"; then
    pass "output contains 'foo' and 'bar'"
else
    fail "expected 'foo' and 'bar', got: $(echo "$OUT" | grep -v '^\[')"
fi

# ---------------------------------------------------------------------------
# Test 4: non-zero exit propagation
# ---------------------------------------------------------------------------

echo ""
echo "=== test 4: exit code propagation ==="
pelagos run alpine /bin/false > /dev/null 2>&1; EXIT=$?
if [ "$EXIT" -eq 1 ]; then
    pass "exit code 1 propagated correctly"
else
    fail "expected exit 1, got $EXIT"
fi

# ---------------------------------------------------------------------------
# Test 5: back-to-back runs
# ---------------------------------------------------------------------------

echo ""
echo "=== test 5: three back-to-back runs ==="
BACK_FAIL=0
for i in 1 2 3; do
    OUT=$(pelagos run alpine /bin/echo "run$i")
    if echo "$OUT" | grep -q "^run${i}$"; then
        echo "  [OK]   run $i: ok"
    else
        echo "  [FAIL] run $i: expected 'run${i}', got: $(echo "$OUT" | grep -v '^\[')"
        BACK_FAIL=$((BACK_FAIL + 1))
    fi
done
if [ "$BACK_FAIL" -eq 0 ]; then
    pass "all 3 back-to-back runs succeeded"
else
    fail "$BACK_FAIL of 3 back-to-back runs failed"
fi

fi  # end of functional tests

# ---------------------------------------------------------------------------
# Daemon lifecycle tests (cold and warm modes)
# ---------------------------------------------------------------------------

if [ "$MODE" = "cold" ] || [ "$MODE" = "warm" ]; then

# ---------------------------------------------------------------------------
# Test 6: vm status reports running
# ---------------------------------------------------------------------------

echo ""
echo "=== test 6: vm status ==="
OUT=$(pelagos vm status 2>&1 || true)
echo "  $OUT"
if echo "$OUT" | grep -q "running"; then
    pass "vm status reports running"
else
    fail "expected 'running', got: $OUT"
fi

# ---------------------------------------------------------------------------
# Test 7: warm ping is fast (daemon already running)
# ---------------------------------------------------------------------------

echo ""
echo "=== test 7: warm ping latency ==="
T0=$(ms_now); OUT=$(pelagos ping); T1=$(ms_now)
WARM_MS=$(( T1 - T0 ))
echo "$OUT" | grep -v "^\["
echo "  [TIME] warm ping: ${WARM_MS} ms"
if echo "$OUT" | grep -q "^pong$"; then
    # Warm run should complete in under 3 seconds (generous allowance)
    if [ "$WARM_MS" -lt 3000 ]; then
        pass "warm ping: ${WARM_MS} ms (< 3000 ms)"
    else
        fail "warm ping too slow: ${WARM_MS} ms (expected < 3000 ms)"
    fi
else
    fail "warm ping did not return 'pong'"
fi

if [ "$MODE" = "cold" ]; then
    echo "  [INFO] cold-start: ${COLD_MS} ms  →  warm: ${WARM_MS} ms"
fi

# ---------------------------------------------------------------------------
# Test 8: vm stop + verify stopped
# ---------------------------------------------------------------------------

echo ""
echo "=== test 8: vm stop ==="
pelagos vm stop > /dev/null 2>&1
sleep 2
OUT=$(pelagos vm status 2>&1 || true)
echo "  $OUT"
if echo "$OUT" | grep -q "stopped"; then
    pass "daemon stopped after 'vm stop'"
else
    fail "expected 'stopped', got: $OUT"
fi

fi  # end of daemon lifecycle tests

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "================================"
if [ "$FAIL" -eq 0 ]; then
    echo "PASS  ($PASS tests passed)"
    exit 0
else
    echo "FAIL  ($FAIL failed, $PASS passed)"
    echo ""
    echo "If image pulls are failing with 'error sending request', PF state"
    echo "has degraded. Fix with:  sudo pfctl -f /etc/pf.conf"
    exit 1
fi
