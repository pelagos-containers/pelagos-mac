#!/usr/bin/env bash
# test-e2e.sh — Integration test: boot VM and verify vsock ping round-trip.
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0 rdinit=/pelagos-init"

PASS=0
FAIL=0

check_file() {
    local label="$1" path="$2"
    if [ -f "$path" ]; then
        echo "  [OK]  $label: $path"
    else
        echo "  [FAIL] $label missing: $path"
        FAIL=$((FAIL + 1))
    fi
}

echo "=== pelagos e2e: preflight checks ==="
check_file "kernel"     "$KERNEL"
check_file "initramfs"  "$INITRD"
check_file "disk"       "$DISK"
check_file "binary"     "$BINARY"

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "FAIL: $FAIL preflight check(s) failed. Run 'make image' and 'make sign' first."
    exit 1
fi

echo ""
echo "=== pelagos e2e: ping test ==="
OUTPUT=$("$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    ping 2>&1) || true

echo "Output: $OUTPUT"

if echo "$OUTPUT" | grep -q "pong"; then
    PASS=$((PASS + 1))
    echo "  [OK]  output contains 'pong'"
else
    FAIL=$((FAIL + 1))
    echo "  [FAIL] output does not contain 'pong'"
fi

echo ""
if [ "$FAIL" -eq 0 ]; then
    echo "PASS ($PASS check(s))"
    exit 0
else
    echo "FAIL ($FAIL check(s) failed, $PASS passed)"
    exit 1
fi
