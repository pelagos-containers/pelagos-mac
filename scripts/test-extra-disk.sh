#!/usr/bin/env bash
# test-extra-disk.sh — Manual smoke tests for the --extra-disk feature.
#
# Levels:
#   1  CLI parses --extra-disk without error
#   2  Daemon subprocess receives the flag (check ps argv)
#   3  VM sees /dev/vdb when booted with --extra-disk
#
# Usage:
#   bash scripts/test-extra-disk.sh [--level 1|2|3]
#   (default: run all three levels)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
EXTRA_IMG="/tmp/pelagos-test-extra.img"
PELAGOS_BASE="${XDG_DATA_HOME:-$HOME/.local/share}/pelagos"

LEVEL_MAX=3
if [[ "${1:-}" == "--level" ]]; then
    LEVEL_MAX="$2"
fi

PASS=0
FAIL=0

result() { local s="$1" msg="$2"; if [[ "$s" == "ok" ]]; then echo "  PASS: $msg"; ((PASS++)); else echo "  FAIL: $msg"; ((FAIL++)); fi; }

# ---------------------------------------------------------------------------
# Level 1 — CLI parses --extra-disk
# ---------------------------------------------------------------------------

echo ""
echo "=== Level 1: CLI parsing ==="

if "$BINARY" --extra-disk /tmp/nonexistent.img --help 2>&1 | grep -q "extra-disk"; then
    result ok "--extra-disk appears in --help output"
else
    result fail "--extra-disk not found in --help output"
fi

if "$BINARY" --extra-disk /tmp/a.img --extra-disk /tmp/b.img --help 2>&1 | grep -q "extra-disk"; then
    result ok "multiple --extra-disk flags accepted by clap"
else
    result fail "multiple --extra-disk flags rejected"
fi

[[ "$LEVEL_MAX" -lt 2 ]] && { echo ""; echo "Levels 1-$LEVEL_MAX complete: $PASS pass, $FAIL fail"; exit $((FAIL > 0)); }

# ---------------------------------------------------------------------------
# Level 2 — Daemon subprocess argv contains --extra-disk
# ---------------------------------------------------------------------------

echo ""
echo "=== Level 2: daemon subprocess argv ==="

for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [[ ! -f "$f" ]]; then
        echo "  SKIP: $f not found — run build-vm-image.sh first"
        exit 0
    fi
done

# Stop any running VM daemon — extra disks cannot be added to a live VM.
echo "  stopping any running VM daemon..."
pkill -TERM -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
sleep 2
pkill -KILL -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
rm -f "$PELAGOS_BASE/vm.pid" "$PELAGOS_BASE/vm.sock"

# Create a tiny sparse extra disk just for the boot test.
dd if=/dev/zero of="$EXTRA_IMG" bs=1m count=0 seek=10 2>/dev/null
echo "  created test extra disk: $EXTRA_IMG (10 MB sparse)"

# Boot the VM with the extra disk.
echo "  booting VM with --extra-disk..."
"$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" --extra-disk "$EXTRA_IMG" ping > /tmp/ping-out.txt 2>&1 &
PING_PID=$!
sleep 3

# Check the daemon's argv.
if ps aux | grep "vm-daemon-internal" | grep -v grep | grep -q "extra-disk"; then
    result ok "daemon subprocess argv contains --extra-disk"
else
    result fail "daemon subprocess argv does not contain --extra-disk"
    echo "  daemon argv: $(ps aux | grep vm-daemon-internal | grep -v grep || echo '(not found)')"
fi

wait "$PING_PID" 2>/dev/null || true
if grep -q pong /tmp/ping-out.txt; then
    result ok "VM booted and responded to ping"
else
    result fail "VM did not respond to ping"
fi

[[ "$LEVEL_MAX" -lt 3 ]] && { echo ""; echo "Levels 1-$LEVEL_MAX complete: $PASS pass, $FAIL fail"; exit $((FAIL > 0)); }

# ---------------------------------------------------------------------------
# Level 3 — VM sees /dev/vdb
# ---------------------------------------------------------------------------

echo ""
echo "=== Level 3: /dev/vdb visible inside VM ==="

# Use pelagos vm ssh (goes through the daemon/vsock/smoltcp relay) rather
# than direct SSH — 192.168.105.2 is not directly routable from macOS with
# the smoltcp NAT relay.
VDISKS=$("$BINARY" --kernel "$KERNEL" --initrd "$INITRD" --disk "$DISK" \
    --extra-disk "$EXTRA_IMG" vm ssh -- "ls /dev/vd* 2>/dev/null" 2>/dev/null || echo "")
if echo "$VDISKS" | grep -q vdb; then
    result ok "/dev/vdb present inside VM ($VDISKS)"
else
    result fail "/dev/vdb not found inside VM (saw: ${VDISKS:-nothing})"
fi

# Cleanup: stop the provisioning VM so the normal Alpine VM can be restarted.
echo ""
echo "  stopping test VM..."
pkill -TERM -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
sleep 2
pkill -KILL -f "pelagos.*vm-daemon-internal" 2>/dev/null || true
rm -f "$PELAGOS_BASE/vm.pid" "$PELAGOS_BASE/vm.sock"
rm -f "$EXTRA_IMG"
echo "  cleaned up"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "=== Results: $PASS pass, $FAIL fail ==="
exit $((FAIL > 0))
