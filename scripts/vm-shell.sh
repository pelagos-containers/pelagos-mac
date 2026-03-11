#!/usr/bin/env bash
# vm-shell.sh — Open an interactive shell directly inside the Linux VM.
#
# Unlike scripts/test-interactive.sh (which runs a container via pelagos exec),
# this drops you into the VM's own environment — no container namespaces, no
# overlay filesystem, full access to the VM's processes and kernel interfaces.
#
# Usage:
#   ./scripts/vm-shell.sh
#
# Prerequisites:
#   - make image   (builds out/vmlinuz, out/initramfs-custom.gz, out/root.img)
#   - make sign    (builds and signs target/aarch64-apple-darwin/release/pelagos)
#
# What you get:
#   - /bin/sh (busybox ash) running as PID 1's child in the VM
#   - Access to /proc, /sys, /dev, /run/pelagos (container state)
#   - socket_vmnet network interface visible via 'busybox ip addr'
#   - pelagos-guest process visible via 'busybox ps'
#   - No container isolation — you are in the raw VM init namespace
#
# Note: the initramfs does not symlink all busybox applets. Use the 'busybox'
# prefix for commands that are not found (e.g. 'busybox ps', 'busybox ip').

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPT_DIR")"

KERNEL="$REPO_ROOT/out/vmlinuz"
INITRD="$REPO_ROOT/out/initramfs-custom.gz"
DISK="$REPO_ROOT/out/root.img"
BINARY="$REPO_ROOT/target/aarch64-apple-darwin/release/pelagos"
CMDLINE="console=hvc0"

# Verify required artifacts exist.
for f in "$KERNEL" "$INITRD" "$DISK" "$BINARY"; do
    if [ ! -f "$f" ]; then
        echo "Missing: $f" >&2
        echo "Run 'make image' and 'make sign' first." >&2
        exit 1
    fi
done

exec "$BINARY" \
    --kernel  "$KERNEL" \
    --initrd  "$INITRD" \
    --disk    "$DISK" \
    --cmdline "$CMDLINE" \
    vm shell
