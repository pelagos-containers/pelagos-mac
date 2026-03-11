#!/usr/bin/env bash
# test-e2e.sh — End-to-end integration tests for pelagos-mac.
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

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

echo "=== preflight ==="
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
# Test 1: ping
# ---------------------------------------------------------------------------

echo ""
echo "=== test 1: ping ==="
OUT=$(pelagos ping)
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
# Test 5: back-to-back runs (teardown fix)
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
